use std::collections::HashMap;
use std::io::{BufReader, BufRead};
use std::net::IpAddr;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::thread;

use anyhow::{Result, Context};
use crossbeam::channel::{unbounded, Receiver, Sender};
use ipnet::{IpAddrRange, IpNet};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

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
    errors_sender:  Option<Sender<anyhow::Result<()>>>,
    errors_receiver: Receiver<anyhow::Result<()>>,
    revert: bool,
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
    ns: Namespace,
    veth: Veth,
    qdisc: Option<Qdisc>,
    command: String,
}

impl<'a> Builder<'a> {
    fn new(env: &'a mut Env, cmd: String) -> Self {
        let id = env.next;
        let ip = env.hosts.peekable().peek().map(|ip| *ip).unwrap();
        let ns = Namespace::new(&env.prefix, id);
        let veth = Veth::new(ip, env.bridge.as_ref().unwrap().clone(), ns.clone());
        Builder {
            env,
            ns,
            veth,
            qdisc: None,
            command: cmd,
        }
    }

    pub fn with_qdisc(mut self, tbf: Option<String>, netem: Option<String>) -> Self {
        if tbf.is_none() && netem.is_none() {
            return self;
        }
        self.qdisc = Some(Qdisc::new(self.veth.clone(), tbf, netem));
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
        let mut task = Task::new(self.ns.clone(), self.command);
        task.spawn(sender)?;
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
        shell(&format!("ip -n {} addr add {} dev {}", self.namespace.name, self.addr(), self.guest()))?;
        shell(&format!("ip -n {} link set {} up", self.namespace.name, self.guest()))?;
        shell(&format!("ip link set {} up", self.host()))?;
        Ok(())
    }

    fn revert(&self) -> Result<()> {
        shell(&format!("ip link del {}", self.guest()))?;
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
                Some(_) => "parent handle 1 handle 10",
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
            shell(&format!(
                "ip netns exec {} tc qdisc del dev {} root",
                self.veth.namespace.name,
                self.veth.guest()
            ))
        } else {
            Ok(())
        }
    }
}

fn shell(cmd: &str) -> Result<()> {
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
    Ok(())
}

fn random_suffix(n: usize) -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

struct Task {
    ns: Namespace,
    cmd: String,
    handlers: Vec<thread::JoinHandle<()>>,
    process: Option<Child>,
}

impl Task {
    fn new(ns: Namespace, cmd: String) -> Self {
        Task {
            ns,
            cmd,
            handlers: vec![],
            process: None,
        }
    }

    fn command(&self) -> String {
        format!("ip netns exec {} {}", self.ns.name, self.cmd)
    }

    fn spawn(&mut self, errors_sender: Sender<Result<()>>) -> Result<()> {
        let cmd = self.command();
        let mut splitted = cmd.split_whitespace();
        let first = splitted
            .next()
            .ok_or_else(|| anyhow::anyhow!("no command found in the command string: {}", cmd))?;
        let mut shell = Command::new(first)
            .args(splitted)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn command")?;
        let stdout = shell
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to take stdout from child process"))?;

        let stderr = shell
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to take stderr from child process"))?;

        let id = self.ns.name.clone();
        let sender = errors_sender.clone();
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
        let sender= errors_sender.clone();
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
        self.process = Some(shell);
        self.handlers.push(stdout_handler);
        self.handlers.push(stderr_handler);
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
