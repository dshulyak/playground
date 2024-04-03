use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::{env, thread};

use anyhow::{Context, Result};
use crossbeam::channel::{unbounded, Receiver, Sender};
use ipnet::{IpAddrRange, IpNet};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use serde_json::Value;

trait Actionable {
    fn apply(&self) -> Result<()>;
    fn revert(&self) -> Result<()>;
}

pub struct Env {
    prefix: String,
    hosts: IpAddrRange,
    next: usize,
    bridge: Option<Bridge>,
    namespaces: HashMap<String, Namespace>,
    veth: HashMap<String, Veth>,
    qdisc: HashMap<String, Qdisc>,
    commands: HashMap<String, Task>,
    errors_sender: Option<Sender<anyhow::Result<()>>>,
    errors_receiver: Receiver<anyhow::Result<()>>,
    revert: bool,
    // redirect stdout and stderr to files in the working directories
    redirect: bool,
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
            commands: HashMap::new(),
            errors_sender: Some(errors_sender),
            errors_receiver,
            revert: true,
            redirect: false,
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

    pub fn run(mut self, f: impl FnOnce(&mut Self)) -> Result<()> {
        let rst = {
            let bridge = Bridge::new(&self.prefix);
            let rst = bridge.apply();
            self.bridge = Some(bridge);
            if rst.is_err() {
                return rst;
            }
            f(&mut self);
            Ok(())
        };
        for (_, mut task) in self.commands.drain() {
            if let Err(err) = task.stop() {
                tracing::debug!("failed to stop task: {:?}", err);
            }
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
        self.env.commands.insert(id.clone(), task);
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Namespace {
    name: String,
}

impl Namespace {
    fn new(prefix: &str, index: usize) -> Self {
        Self {
            name: format!("{}-{}", prefix, index),
        }
    }

    pub fn cleanup(prefix: &str) -> Result<usize> {
        let output = shell("ip -json netns list")?;
        let namespaces: Vec<HashMap<String, Value>> = serde_json::from_slice(&output)?;
        let mut count = 0;
        for ns in namespaces {
            match ns["name"] {
                Value::String(ref name) if name.starts_with(prefix) => {
                    shell(&format!("ip netns del {}", name))?;
                    count += 1;
                }
                _ => {}
            }
        }
        Ok(count)
    }
}

impl Actionable for Namespace {
    fn apply(&self) -> Result<()> {
        shell(&format!("ip netns add {}", self.name))?;
        shell(&format!("ip netns exec {} ip link set lo up", self.name))?;
        Ok(())
    }

    fn revert(&self) -> Result<()> {
        shell(&format!("ip netns del {}", self.name))?;
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

#[derive(Debug, Clone)]
struct Bridge {
    name: String,
}

impl Bridge {
    fn new(ns: &str) -> Self {
        Bridge {
            name: format!("{}-br", ns),
        }
    }

    pub fn cleanup(prefix: &str) -> Result<usize> {
        let output = shell("ip -json link show type bridge")?;
        let bridges: Vec<HashMap<String, Value>> =
            serde_json::from_slice(&output).context("decode bridges")?;
        let mut count = 0;
        for bridge in bridges {
            match &bridge["ifname"] {
                Value::String(ifname) if ifname.starts_with(prefix) => {
                    shell(&format!("ip link del {}", ifname))?;
                    count += 1;
                }
                _ => {}
            }
        }
        Ok(count)
    }
}

impl Actionable for Bridge {
    fn apply(&self) -> Result<()> {
        shell(&format!("ip link add {} type bridge", self.name))?;
        shell(&format!("ip link set {} up", self.name))?;
        Ok(())
    }

    fn revert(&self) -> Result<()> {
        shell(&format!("ip link del {}", self.name))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Veth {
    addr: IpAddr,
    bridge: Bridge,
    namespace: Namespace,
}

impl Veth {
    fn new(addr: IpAddr, bridge: Bridge, namespace: Namespace) -> Self {
        Veth {
            addr,
            bridge,
            namespace,
        }
    }

    fn addr(&self) -> String {
        match self.addr {
            IpAddr::V4(addr) => format!("{}/24", addr),
            IpAddr::V6(addr) => format!("{}/64", addr),
        }
    }

    fn guest(&self) -> String {
        format!("veth-{}-ns", self.namespace.name)
    }

    fn host(&self) -> String {
        format!("veth-{}-br", self.namespace.name)
    }

    pub fn cleanup(prefix: &str) -> Result<usize> {
        let output = shell("ip -json link show type veth")?;
        let veths: Vec<HashMap<String, Value>> = serde_json::from_slice(&output)?;
        let mut count = 0;
        for veth in veths {
            match &veth["ifname"] {
                Value::String(ifname) if ifname.starts_with(prefix) => {
                    shell(&format!("ip link del {}", ifname))?;
                    count += 1;
                }
                _ => {}
            }
        }
        Ok(count)
    }
}

impl Actionable for Veth {
    fn apply(&self) -> Result<()> {
        shell(&format!(
            "ip link add {} type veth peer name {}",
            self.guest(),
            self.host()
        ))?;
        shell(&format!(
            "ip link set {} netns {}",
            self.guest(),
            self.namespace.name
        ))?;
        shell(&format!(
            "ip link set {} master {}",
            self.host(),
            self.bridge.name
        ))?;
        shell(&format!(
            "ip -n {} addr add {} dev {}",
            self.namespace.name,
            self.addr(),
            self.guest()
        ))?;
        shell(&format!(
            "ip -n {} link set {} up",
            self.namespace.name,
            self.guest()
        ))?;
        shell(&format!("ip link set {} up", self.host()))?;
        Ok(())
    }

    fn revert(&self) -> Result<()> {
        shell(&format!(
            "ip -n {} link del {}",
            self.namespace.name,
            self.guest()
        ))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Qdisc {
    veth: Veth,
    tbf: Option<String>,
    netem: Option<String>,
}

impl Qdisc {
    fn new(veth: Veth, tbf: Option<String>, netem: Option<String>) -> Self {
        Qdisc { veth, tbf, netem }
    }
}

impl Actionable for Qdisc {
    fn apply(&self) -> Result<()> {
        if let Some(tbf) = &self.tbf {
            shell(&format!(
                "ip netns exec {} tc qdisc add dev {} root handle 1: tbf {}",
                self.veth.namespace.name,
                self.veth.guest(),
                tbf
            ))?;
        }
        if let Some(netem) = &self.netem {
            let handle = match self.tbf {
                None => "root handle 1",
                Some(_) => "parent 1:1 handle 10",
            };
            shell(&format!(
                "ip netns exec {} tc qdisc add dev {} {}: netem {}",
                self.veth.namespace.name,
                self.veth.guest(),
                handle,
                netem
            ))?;
        }
        Ok(())
    }

    fn revert(&self) -> Result<()> {
        if self.netem.is_some() || self.tbf.is_some() {
            if let Err(err) = shell(&format!(
                "ip netns exec {} tc qdisc del dev {} root",
                self.veth.namespace.name,
                self.veth.guest()
            )) {
                return Result::Err(err);
            }
        }
        Ok(())
    }
}

fn shell(cmd: &str) -> Result<Vec<u8>> {
    tracing::debug!("running: {}", cmd);
    let mut parts = cmd.split_whitespace();
    let command = parts.next().unwrap().to_string();
    let args: Vec<_> = parts.map(|s| s.to_string()).collect();

    let shell = Command::new(command)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let output = shell.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "{}. stderr: {}",
            cmd,
            String::from_utf8(output.stderr).expect("invalid utf8")
        )
    }

    Ok(output.stdout)
}

fn random_suffix(n: usize) -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

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
        if let Some(process) = &mut self.process {
            process.kill()?;
            match process.wait() {
                Ok(status) => {
                    if !status.success() {
                        anyhow::bail!("command failed with status: {}", status);
                    }
                }
                Err(err) => {
                    anyhow::bail!("failed to wait for command: {:?}", err);
                }
            }
        }
        Ok(())
    }
}
