use std::collections::HashMap;
use std::fmt::Display;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

use anyhow::{Context, Result};
use crossbeam::channel::{unbounded, Receiver, Sender};
use crossbeam::select;
use ipnet::{IpAddrRange, IpNet};
use partition::{PartitionBackground, PartitionTask};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

mod network;
pub mod partition;
mod periodic;

use crate::network::{Bridge, Namespace, Qdisc, Veth};
use crate::periodic::{MinInstantEntry, MinInstantHeap};

pub struct Env {
    prefix: String,
    hosts: IpAddrRange,
    next: usize,
    bridge: Option<Bridge>,
    namespaces: HashMap<String, Namespace>,
    veth: HashMap<String, Veth>,
    qdisc: HashMap<String, Qdisc>,
    tasks: HashMap<String, Arc<Mutex<Task>>>,
    errors_sender: Option<Sender<anyhow::Result<()>>>,
    errors_receiver: Receiver<anyhow::Result<()>>,
    revert: bool,
    // redirect stdout and stderr to files in the working directories
    redirect: bool,
    shutdown_actor: Arc<ShutdownActor>,
    partition: Option<PartitionBackground>,
}

impl Env {
    pub fn new() -> Self {
        let (errors_sender, errors_receiver) = unbounded();
        Env {
            prefix: format!("p-{}", random_suffix(4)),
            hosts: IpNet::from_str("10.0.0.0/16").unwrap().hosts(),
            next: 0,
            bridge: None,
            namespaces: HashMap::new(),
            veth: HashMap::new(),
            qdisc: HashMap::new(),
            tasks: HashMap::new(),
            errors_sender: Some(errors_sender),
            errors_receiver,
            revert: true,
            redirect: false,
            shutdown_actor: Arc::new(ShutdownActor::new()),
            partition: None,
        }
    }

    pub fn with_prefix(mut self, prefix: String) -> Self {
        self.prefix = prefix;
        self
    }

    pub fn with_network(mut self, network: IpNet) -> Self {
        self.hosts = network.hosts();
        self
    }

    pub fn with_revert(mut self, revert: bool) -> Self {
        self.revert = revert;
        self
    }

    pub fn with_redirect(mut self, redirect: bool) -> Self {
        self.redirect = redirect;
        self
    }

    pub fn add(&mut self, cmd: String) -> Builder {
        Builder::new(self, cmd)
    }

    pub fn errors(&self) -> &Receiver<anyhow::Result<()>> {
        &self.errors_receiver
    }

    pub fn enable_partition(&mut self, partition: partition::Partition) -> Result<()> {
        let task = PartitionTask::new(partition, self.veth.values().cloned().collect());
        self.partition = Some(PartitionBackground::spawn(task)?);
        Ok(())
    }

    pub fn run(mut self, f: impl FnOnce(&mut Self)) -> Result<()> {
        let background = self.shutdown_actor.clone();
        thread::spawn(move || {
            background.run();
        });
        let rst = {
            let ip = self
                .hosts
                .next()
                .ok_or_else(|| anyhow::anyhow!("no ip address for bridge"))?;
            let bridge = Bridge::new(&self.prefix, ip);
            let rst = bridge.apply();
            self.bridge = Some(bridge);
            if rst.is_err() {
                return rst;
            }
            f(&mut self);
            Ok(())
        };
        for (_, task) in self.tasks.drain() {
            if let Err(err) = task.lock().unwrap().stop() {
                tracing::debug!("failed to stop task: {:?}", err);
            }
        }
        if let Some(partition) = self.partition.take() {
            partition.stop();
        }
        if self.revert {
            for (_, veth) in self.veth.drain() {
                if let Err(err) = veth.revert() {
                    tracing::debug!("failed to revert veth: {:?}", err);
                };
            }
            for (_, namespace) in self.namespaces.drain() {
                if let Err(err) = namespace.revert() {
                    tracing::debug!("failed to revert namespace: {:?}", err);
                };
            }
            if let Some(bridge) = self.bridge.take() {
                if let Err(err) = bridge.revert() {
                    tracing::debug!("failed to revert bridge: {:?}", err);
                };
            }
        }
        rst
    }
}

pub struct Builder<'a> {
    env: &'a mut Env,
    index: usize,
    ns: Namespace,
    veth: Veth,
    qdisc: Option<Qdisc>,
    command: String,
    work_dir: Option<PathBuf>,
    os_env: HashMap<String, String>,
    shutdown: Option<Shutdown>,
}

impl<'a> Builder<'a> {
    fn new(env: &'a mut Env, cmd: String) -> Self {
        let index = env.next;
        let ip = env.hosts.peekable().peek().map(|ip| *ip).unwrap();
        let ns = Namespace::new(&env.prefix, index);
        let veth = Veth::new(ip, env.bridge.as_ref().unwrap().clone(), ns.clone());
        Builder {
            env,
            index,
            ns,
            veth,
            qdisc: None,
            command: cmd,
            work_dir: None,
            os_env: HashMap::new(),
            shutdown: None,
        }
    }

    pub fn with_qdisc(mut self, tbf: Option<String>, netem: Option<String>) -> Self {
        if tbf.is_none() && netem.is_none() {
            return self;
        }
        self.qdisc = Some(Qdisc::new(self.veth.clone(), tbf, netem));
        self
    }

    pub fn with_work_dir(mut self, work_dir: PathBuf) -> Self {
        self.work_dir = Some(work_dir);
        self
    }

    pub fn with_os_env(mut self, key: String, value: String) -> Self {
        self.os_env.insert(key, value);
        self
    }

    pub fn with_shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = Some(shutdown);
        self
    }

    pub fn spawn(self) -> anyhow::Result<()> {
        let sender = match &self.env.errors_sender {
            Some(sender) => sender.clone(),
            None => anyhow::bail!("can't spawn new tasks after the environment has been run"),
        };
        _ = self.env.hosts.next();
        self.env.next += 1;
        let id = self.ns.name.clone();
        self.env.namespaces.insert(id.clone(), self.ns.clone());
        self.ns.apply()?;
        self.env.veth.insert(id.clone(), self.veth.clone());
        self.veth.apply()?;
        if let Some(qdisc) = self.qdisc {
            self.env.qdisc.insert(id.clone(), qdisc.clone());
            qdisc.apply()?;
        }
        let current_dir = env::current_dir().context("failed to get current directory")?;
        let work_dir = self.work_dir.as_ref().unwrap_or_else(|| &current_dir);
        let mut task = Task::new(
            self.index,
            self.ns.clone(),
            self.command,
            self.env.redirect,
            sender,
            work_dir.clone(),
            self.os_env,
        );
        task.start()?;
        let task = Arc::new(Mutex::new(task));
        if let Some(shutdown) = self.shutdown {
            self.env.shutdown_actor.send(ShutdownTask {
                task: task.clone(),
                shutdown,
            });
        }
        self.env.tasks.insert(id.clone(), task);
        Ok(())
    }
}

pub fn cleanup_bridges(prefix: &str) -> Result<usize> {
    Bridge::cleanup(prefix)
}

pub fn cleanup_veth(prefix: &str) -> Result<usize> {
    Veth::cleanup(prefix)
}

pub fn cleanup_namespaces(prefix: &str) -> Result<usize> {
    Namespace::cleanup(prefix)
}

fn random_suffix(n: usize) -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

#[derive(Debug)]
struct Task {
    index: usize,
    ns: Namespace,
    cmd: String,
    redirect: bool,
    handlers: Vec<thread::JoinHandle<()>>,
    process: Option<Child>,
    errors_sender: Sender<Result<()>>,
    work_dir: PathBuf,
    env: HashMap<String, String>,
}

impl Task {
    fn new(
        index: usize,
        ns: Namespace,
        cmd: String,
        redirect: bool,
        errors_sender: Sender<Result<()>>,
        work_dir: PathBuf,
        env: HashMap<String, String>,
    ) -> Self {
        Task {
            index,
            ns,
            cmd,
            handlers: vec![],
            process: None,
            redirect,
            errors_sender,
            work_dir,
            env,
        }
    }

    fn command(&self) -> String {
        // replace index in the command
        let replaced = self.cmd.replace("{index}", &self.index.to_string());
        format!("ip netns exec {} {}", self.ns.name, replaced)
    }

    fn start(&mut self) -> Result<()> {
        let cmd = self.command();
        let mut splitted = cmd.split_whitespace();
        let first = splitted
            .next()
            .ok_or_else(|| anyhow::anyhow!("no command found in the command string: {}", cmd))?;

        let mut shell = Command::new(first);
        shell.args(splitted);
        shell.current_dir(&self.work_dir);
        if !self.redirect {
            shell.stdout(Stdio::piped()).stderr(Stdio::piped());
        } else {
            let stdout = OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.work_dir.join(format!("{}.stdout", self.ns.name)))?;
            let stderr = OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.work_dir.join(format!("{}.stderr", self.ns.name)))?;
            shell.stdout(stdout).stderr(stderr);
        }

        for (key, value) in &self.env {
            shell.env(key, value);
        }

        let mut shell = shell.spawn().context("failed to spawn command")?;

        if !self.redirect {
            let stdout = shell
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("failed to take stdout from child process"))?;

            let stderr = shell
                .stderr
                .take()
                .ok_or_else(|| anyhow::anyhow!("failed to take stderr from child process"))?;

            let id = self.ns.name.clone();
            let sender = self.errors_sender.clone();
            let stdout_handler = thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            tracing::info!("[{}]: {}", id, line);
                        }
                        Err(e) => {
                            let _ = sender.send(Err(e.into()));
                            return;
                        }
                    }
                }
            });
            let id = self.ns.name.clone();
            let sender = self.errors_sender.clone();
            let stderr_handler = thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            tracing::info!("[{}]: {}", id, line);
                        }
                        Err(e) => {
                            let _ = sender.send(Err(e.into()));
                            return;
                        }
                    }
                }
            });
            self.handlers.push(stdout_handler);
            self.handlers.push(stderr_handler);
        }

        self.process = Some(shell);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(process) = &mut self.process.take() {
            process.kill().context("kill process")?;
            match process.wait() {
                Ok(status) if status.code().is_none() => {
                    tracing::debug!("command was terminated by signal: {}", status);
                }
                Ok(status) => {
                    if !status.success() {
                        anyhow::bail!("command failed with status: {}", status);
                    }
                }
                Err(err) => {
                    anyhow::bail!("failed to wait for command: {:?}", err);
                }
            }
            for handler in self.handlers.drain(..) {
                _ = handler.join();
            }
        }
        Ok(())
    }
}

impl Display for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.ns.name)
    }
}

#[derive(Debug)]
struct ShutdownTask {
    task: Arc<Mutex<Task>>,
    shutdown: Shutdown,
}

impl Display for ShutdownTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.task.lock().unwrap(), self.shutdown)
    }
}

#[derive(Debug, Clone)]
pub struct Shutdown {
    interval: Duration,
    interval_jitter: Option<Duration>,
    pause: Option<Duration>,
    pause_jitter: Option<Duration>,
}

impl Shutdown {
    pub fn parse(input: &str) -> Result<Shutdown> {
        tracing::debug!("parsing shutdown: {}", input);

        let mut parts = input.split_whitespace();
        let interval = match parts.next() {
            Some("interval") => {
                let interval = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("interval is required"))?;
                humantime::parse_duration(interval)?
            }
            Some(other) => return Err(anyhow::anyhow!("unknown keyword: {}", other)),
            None => return Err(anyhow::anyhow!("interval is required")),
        };
        let token = parts.next();
        let interval_jitter = {
            if let Some("jitter") = token {
                let interval_jitter = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("interval jitter is required"))?;
                Some(humantime::parse_duration(interval_jitter)?)
            } else {
                None
            }
        };
        let pause = match token {
            Some("pause") => {
                let pause = parts
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("pause is required"))?;
                Some(humantime::parse_duration(pause)?)
            }
            Some(other) => return Err(anyhow::anyhow!("unknown keyword: {}", other)),
            None => None,
        };
        let pause_jitter = {
            if pause.is_none() {
                None
            } else {
                match parts.next() {
                    Some("jitter") => {
                        let pause_jitter = parts
                            .next()
                            .ok_or_else(|| anyhow::anyhow!("pause jitter is required"))?;
                        Some(humantime::parse_duration(pause_jitter)?)
                    }
                    Some(other) => return Err(anyhow::anyhow!("unknown keyword: {}", other)),
                    None => None,
                }
            }
        };
        Ok(Shutdown {
            interval,
            interval_jitter,
            pause,
            pause_jitter,
        })
    }
}

impl Display for Shutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interval {}", humantime::Duration::from(self.interval))?;
        if let Some(interval_jitter) = self.interval_jitter {
            write!(f, " jitter {}", humantime::Duration::from(interval_jitter))?;
        }
        if let Some(pause) = self.pause {
            write!(f, " pause {}", humantime::Duration::from(pause))?;
            if let Some(pause_jitter) = self.pause_jitter {
                write!(f, " jitter {}", humantime::Duration::from(pause_jitter))?;
            }
        }
        Ok(())
    }
}

struct ActorState {
    recv: Receiver<ShutdownTask>,
    to_pause: MinInstantHeap<ShutdownTask>,
    to_start: MinInstantHeap<ShutdownTask>,
}

impl ActorState {
    fn on_receive(&mut self, stask: ShutdownTask) {
        tracing::debug!("received shutdown task: {}", stask,);
        self.to_pause.push(MinInstantEntry {
            timestamp: now_with_jitter(stask.shutdown.interval, stask.shutdown.interval_jitter),
            task: stask,
        });
    }

    fn wakeup(&self) -> Option<Duration> {
        let to_pause = self.to_pause.peek().map(|entry| entry.timestamp);
        let to_start = self.to_start.peek().map(|entry| entry.timestamp);
        if let Some(to_pause) = to_pause {
            if let Some(to_start) = to_start {
                return Some(
                    to_pause
                        .min(to_start)
                        .saturating_duration_since(Instant::now()),
                );
            }
        }
        to_pause
            .or(to_start)
            .map(|min| min.saturating_duration_since(Instant::now()))
    }

    fn pause_tasks(&mut self) {
        let now = Instant::now();
        while let Some(entry) = self.to_pause.peek() {
            if entry.timestamp > now {
                break;
            }
            tracing::debug!("pausing task: {}", entry.task);
            let entry = self.to_pause.pop().unwrap();
            let err = { entry.task.task.lock().unwrap().stop() };
            if let Err(err) = err {
                tracing::error!("failed to stop task: {:?}", err);
                self.to_pause.push(MinInstantEntry {
                    timestamp: now_with_jitter(
                        entry.task.shutdown.interval,
                        entry.task.shutdown.interval_jitter,
                    ),
                    task: entry.task,
                });
                continue;
            }
            if let Some(pause) = entry.task.shutdown.pause {
                self.to_start.push(MinInstantEntry {
                    timestamp: now_with_jitter(pause, entry.task.shutdown.pause_jitter),
                    task: entry.task,
                });
            } else {
                tracing::debug!("restarting task: {}", entry.task);
                if let Err(err) = entry.task.task.lock().unwrap().start() {
                    tracing::error!("failed to start task: {:?}", err);
                }
                self.to_pause.push(MinInstantEntry {
                    timestamp: now_with_jitter(
                        entry.task.shutdown.interval,
                        entry.task.shutdown.interval_jitter,
                    ),
                    task: entry.task,
                });
            }
        }
    }

    fn start_tasks(&mut self) {
        let now = Instant::now();
        while let Some(entry) = self.to_start.peek() {
            if entry.timestamp > now {
                break;
            }
            tracing::debug!("starting task: {}", entry.task);
            if let Err(err) = entry.task.task.lock().unwrap().start() {
                tracing::error!("failed to start task: {:?}", err);
                break;
            }
            let entry = self.to_start.pop().unwrap();
            self.to_pause.push(MinInstantEntry {
                timestamp: now_with_jitter(
                    entry.task.shutdown.interval,
                    entry.task.shutdown.interval_jitter,
                ),
                task: entry.task,
            });
        }
    }

    fn run(&mut self) {
        loop {
            match self.wakeup() {
                Some(duration) if duration > Duration::from_nanos(0) => {
                    tracing::debug!("sleeping for {}", humantime::Duration::from(duration));
                    select! {
                        recv(self.recv) -> task => {
                            self.on_receive(task.unwrap());
                        }
                        default(duration) => {}
                    };
                }
                Some(_) => {}
                None => {
                    tracing::debug!("waiting for tasks");
                    select! {
                        recv(self.recv) -> task => {
                            self.on_receive(task.unwrap());
                        }
                    };
                }
            };
            self.pause_tasks();
            self.start_tasks();
        }
    }
}

struct ShutdownActor {
    send: Sender<ShutdownTask>,
    state: Mutex<ActorState>,
}

impl ShutdownActor {
    fn new() -> Self {
        let (send, recv) = unbounded();
        Self {
            send,
            state: Mutex::new(ActorState {
                recv,
                to_pause: MinInstantHeap::new(),
                to_start: MinInstantHeap::new(),
            }),
        }
    }

    fn send(&self, shutdown: ShutdownTask) {
        self.send.send(shutdown).unwrap();
    }

    fn run(&self) {
        self.state.lock().unwrap().run();
    }
}

fn now_with_jitter(duration: Duration, jitter: Option<Duration>) -> Instant {
    let mut rng = thread_rng();
    let mut timestamp = Instant::now() + duration;
    if let Some(jitter) = jitter {
        let jitter = rng.gen_range(-jitter.as_secs_f64()..jitter.as_secs_f64());
        timestamp += Duration::from_secs_f64(jitter);
    };
    timestamp
}
