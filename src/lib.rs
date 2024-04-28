use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::{env, thread, vec};

use anyhow::{Context, Result};
use crossbeam::channel::{unbounded, Receiver, Sender};
use ipnet::{IpAddrRange, IpNet};
use partition::{PartitionBackground, PartitionTask};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

mod network;
pub mod partition;

use crate::network::{Bridge, Namespace, Qdisc, Veth};

pub struct Config {
    prefix: String,
    net: IpNet,
    revert: bool,
    // redirect stdout and stderr to files in the working directories
    redirect: bool,
}

impl Config {
    pub fn new() -> Self {
        Config{
            prefix: format!("p-{}", random_suffix(4)),
            net:  IpNet::from_str("10.0.0.0/16").unwrap(),
            revert: true,
            redirect: false,
        }
    }

    pub fn with_prefix(mut self, prefix: String) -> Self {
        self.prefix = prefix;
        self
    }

    pub fn with_network(mut self, net: IpNet) -> Self {
        self.net = net;
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

    pub fn run(self, f: impl FnOnce(&mut Env)) -> Result<()> {
        let env = Env::new(self);
        env.run(f)
    }
}


pub struct Env {
    cfg: Config,
    hosts: IpAddrRange,
    next: usize,
    data: EnvData,
    bridge: Option<Bridge>,
    errors_sender: Option<Sender<anyhow::Result<()>>>,
    errors_receiver: Receiver<anyhow::Result<()>>,
    partition: Option<PartitionBackground>,
}

impl Env {
    fn new(cfg: Config) -> Self {
        let (errors_sender, errors_receiver) = unbounded();
        let hosts = cfg.net.hosts();
        Env {
            cfg: cfg,
            hosts: hosts,
            next: 0,
            bridge: None,
            data: EnvData::new(),
            errors_sender: Some(errors_sender),
            errors_receiver,
            partition: None,
        }
    }

    pub fn errors(&self) -> &Receiver<anyhow::Result<()>> {
        &self.errors_receiver
    }

    pub fn enable_partition(&mut self, partition: partition::Partition) -> Result<()> {
        let veths = self
            .data
            .network
            .values()
            .map(|network| network.veth.clone())
            .collect();
        let task = PartitionTask::new(partition, veths);
        self.partition = Some(PartitionBackground::spawn(task)?);
        Ok(())
    }

    pub fn add_task(
        &mut self,
        cmd: String,
        work_dir: Option<PathBuf>,
        tbf: Option<String>,
        netem: Option<String>,
        os_env: HashMap<String, String>,
    ) -> anyhow::Result<usize> {
        let index = self.next;
        self.next += 1;
        let ip = self
            .hosts
            .next()
            .ok_or_else(|| anyhow::anyhow!("run out of ip addresses"))?;
        let ns = Namespace::new(&self.cfg.prefix, index);
        let veth = Veth::new(ip, self.bridge.as_ref().unwrap().clone(), ns.clone());
        let current_dir = env::current_dir().context("failed to get current directory")?;
        let work_dir: &PathBuf = work_dir.as_ref().unwrap_or_else(|| &current_dir);

        let network = NetworkData {
            namespace: ns,
            veth: veth.clone(),
            qdisc: if tbf.is_some() || netem.is_some() {
                Some(Qdisc::new(veth.clone(), tbf, netem))
            } else {
                None
            },
        };
        let command = CommandData {
            command: cmd,
            work_dir: work_dir.clone(),
            os_env,
            redirect: self.cfg.redirect,
        };
        self.data.add(index, network, command);
        Ok(index)
    }

    pub fn deploy(&mut self) -> anyhow::Result<()> {
        let data = &mut self.data;

        let network = &data.network;
        let commands = &data.commands;
        let state = &mut data.state;
        let tasks = &mut data.tasks;

        let since = std::time::Instant::now();
        let zipped = state.values_mut().zip(network.values());
        for (state, network) in zipped {
            if let State::Pending = state {
                deploy(network)?;
                *state = State::Deployed;
            }
        }
        tracing::info!("deployed in {:?}", since.elapsed());

        let since = std::time::Instant::now();
        let zipped = state
            .iter_mut()
            .zip(network.values())
            .zip(commands.values());
        for (((index, state), network), command) in zipped {
            let task = run(
                *index,
                network,
                command,
                &self.errors_sender.as_ref().unwrap(),
            )?;
            tasks.insert(*index, task);
            *state = State::Running;
        }
        tracing::info!("commands started in {:?}", since.elapsed());
        Ok(())
    }

    pub fn run(mut self, f: impl FnOnce(&mut Self)) -> Result<()> {
        let rst = {
            let ip = self
                .hosts
                .next()
                .ok_or_else(|| anyhow::anyhow!("no ip address for bridge"))?;
            let bridge = Bridge::new(&self.cfg.prefix, ip);
            let rst = bridge.apply();
            self.bridge = Some(bridge);
            if rst.is_err() {
                return rst;
            }
            f(&mut self);
            Ok(())
        };
        let data = &mut self.data;
        let tasks = &mut data.tasks;
        for task in tasks.iter_mut() {
            if let Err(err) = stop(task.1) {
                tracing::error!("failed to stop task {}: {:?}", task.0, err);
            }
        }
        tasks.clear();

        if let Some(partition) = self.partition.take() {
            partition.stop();
        }
        if self.cfg.revert {
            for (index, network) in data.network.iter() {
                if let Err(err) = cleanup(network) {
                    tracing::error!("failed to cleanup network {}: {:?}", index, err);
                }
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
struct NetworkData {
    namespace: Namespace,
    veth: Veth,
    qdisc: Option<Qdisc>,
}

#[derive(Debug)]
struct CommandData {
    command: String,
    work_dir: PathBuf,
    os_env: HashMap<String, String>,
    redirect: bool,
}

#[derive(Debug)]
enum State {
    Pending,
    Deployed,
    Running,
}

#[derive(Debug)]
struct EnvData {
    network: BTreeMap<usize, NetworkData>,
    commands: BTreeMap<usize, CommandData>,
    tasks: BTreeMap<usize, TaskData>,
    state: BTreeMap<usize, State>,
}

impl EnvData {
    fn new() -> Self {
        Self {
            network: BTreeMap::new(),
            commands: BTreeMap::new(),
            tasks: BTreeMap::new(),
            state: BTreeMap::new(),
        }
    }

    fn add(&mut self, index: usize, network: NetworkData, command: CommandData) -> usize {
        self.network.insert(index, network);
        self.commands.insert(index, command);
        self.state.insert(index, State::Pending);
        index
    }
}

fn deploy(network: &NetworkData) -> anyhow::Result<()> {
    network.namespace.apply()?;
    network.veth.apply()?;
    if let Some(qdisc) = &network.qdisc {
        qdisc.apply()?;
    }
    Ok(())
}

#[derive(Debug)]
struct TaskData {
    output_handlers: Vec<thread::JoinHandle<()>>,
    process: Child,
}

fn run(
    index: usize,
    network: &NetworkData,
    command: &CommandData,
    errors: &Sender<Result<()>>,
) -> anyhow::Result<TaskData> {
    let cmd = command.command.replace("{index}", &index.to_string());
    let cmd = format!("ip netns exec {} {}", network.namespace.name, cmd);

    tracing::debug!(redirect = command.redirect, "running command: {}", cmd);

    let mut splitted = cmd.split_whitespace();
    let first = splitted
        .next()
        .ok_or_else(|| anyhow::anyhow!("no command found in the command string: {}", cmd))?;

    let mut shell = Command::new(first);
    shell.args(splitted);
    shell.current_dir(&command.work_dir);
    if !command.redirect {
        shell.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        let stdout = OpenOptions::new().append(true).create(true).open(
            command
                .work_dir
                .join(format!("{}.stdout", network.namespace.name)),
        )?;
        let stderr = OpenOptions::new().append(true).create(true).open(
            command
                .work_dir
                .join(format!("{}.stderr", network.namespace.name)),
        )?;
        shell.stdout(stdout).stderr(stderr);
    }

    for (key, value) in &command.os_env {
        shell.env(key, value);
    }

    let mut shell = shell.spawn().context("failed to spawn command")?;
    let handlers = if !command.redirect {
        let stdout = shell
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to take stdout from child process"))?;

        let stderr = shell
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to take stderr from child process"))?;

        let id = network.namespace.name.clone();
        let sender = errors.clone();
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
        let id = network.namespace.name.clone();
        let sender = errors.clone();
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
        vec![stdout_handler, stderr_handler]
    } else {
        vec![]
    };
    Ok(TaskData {
        output_handlers: handlers,
        process: shell,
    })
}

fn stop(task: &mut TaskData) -> Result<()> {
    task.process.kill().context("kill process")?;
    match task.process.wait() {
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
    for handler in task.output_handlers.drain(..) {
        _ = handler.join();
    }
    Ok(())
}

fn cleanup(network: &NetworkData) -> anyhow::Result<()> {
    if let Err(err) = network.veth.revert() {
        tracing::debug!("failed to revert veth: {:?}", err);
    }
    if let Err(err) = network.namespace.revert() {
        tracing::debug!("failed to revert namespace: {:?}", err);
    }
    Ok(())
}
