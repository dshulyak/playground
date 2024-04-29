use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::{env, thread, vec};

use anyhow::{ensure, Context, Result};
use crossbeam::channel::{unbounded, Receiver, Sender};
use ipnet::{IpAddrRange, IpNet};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

mod netlink;
mod network;
pub mod partition;
pub mod shell;

use crate::network::{Bridge, Namespace, NamespaceVeth, Qdisc};

// the primary limitation is the limit of ports enforced in the kernel (the limit is 1<<10)
// https://github.com/torvalds/linux/blob/80e62bc8487b049696e67ad133c503bf7f6806f7/net/bridge/br_private.h#L28
// https://github.com/moby/moby/issues/44973#issuecomment-1543733757
// the veth is 0 based
const MAX_VETH_PER_BRIDGE: usize = 1023;

pub struct Config {
    prefix: String,
    net: IpNet,
    instances_per_bridge: usize,
    revert: bool,
    // redirect stdout and stderr to files in the working directories
    redirect: bool,
}

impl Config {
    pub fn new() -> Self {
        Config {
            prefix: format!("p-{}", random_suffix(4)),
            net: IpNet::from_str("10.0.0.0/16").unwrap(),

            instances_per_bridge: MAX_VETH_PER_BRIDGE,
            revert: true,
            redirect: false,
        }
    }

    pub fn with_instances_per_bridge(mut self, instances_per_bridge: usize) -> Result<Self> {
        ensure!(
            instances_per_bridge > 0,
            "instances_per_bridge must be greater than 0"
        );
        ensure!(
            instances_per_bridge <= MAX_VETH_PER_BRIDGE,
            "instances_per_bridge must be less than or equal to {}",
            MAX_VETH_PER_BRIDGE
        );
        self.instances_per_bridge = instances_per_bridge.min(MAX_VETH_PER_BRIDGE);
        Ok(self)
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
}

pub struct Env {
    cfg: Config,
    hosts: IpAddrRange,
    next: usize,
    instances: Instances,
    bridges: BTreeMap<usize, (Bridge, State)>,
    connects: BTreeMap<(usize, usize), State>,
    errors_sender: Sender<anyhow::Result<()>>,
    errors_receiver: Receiver<anyhow::Result<()>>,
    partition: Option<partition::Background>,
}

impl Env {
    pub fn new(cfg: Config) -> Self {
        let (sender, receiver) = unbounded();
        let hosts = cfg.net.hosts();
        Env {
            cfg: cfg,
            hosts: hosts,
            next: 0,
            bridges: BTreeMap::new(),
            connects: BTreeMap::new(),
            instances: Instances::new(),
            errors_sender: sender,
            errors_receiver: receiver,
            partition: None,
        }
    }

    pub fn errors(&self) -> &Receiver<anyhow::Result<()>> {
        &self.errors_receiver
    }

    pub fn enable_partition(&mut self, partition: partition::Partition) -> Result<()> {
        let veths = self
            .instances
            .network
            .values()
            .map(|network| network.veth.clone())
            .collect();
        let task = partition::Task::new(partition, veths);
        self.partition = Some(partition::Background::spawn(task)?);
        Ok(())
    }

    pub fn add(
        &mut self,
        cmd: String,
        work_dir: Option<PathBuf>,
        tbf: Option<String>,
        netem: Option<String>,
        os_env: HashMap<String, String>,
    ) -> anyhow::Result<usize> {
        let index = self.next;
        self.next += 1;
        let bridge_index = index / self.cfg.instances_per_bridge;
        if !self.bridges.contains_key(&bridge_index) {
            let ip = self
                .hosts
                .next()
                .ok_or_else(|| anyhow::anyhow!("run out of ip addresses"))?;
            let bridge = Bridge::new(bridge_index, &self.cfg.prefix, ip);
            self.bridges.insert(bridge_index, (bridge, State::Pending));
            for i in 0..bridge_index {
                self.connects.insert((i, bridge_index), State::Pending);
            }
        }

        let ip = self
            .hosts
            .next()
            .ok_or_else(|| anyhow::anyhow!("run out of ip addresses"))?;
        let ns = Namespace::new(&self.cfg.prefix, index);
        let veth = NamespaceVeth::new(ip, ns.clone());
        let current_dir = env::current_dir().context("failed to get current directory")?;
        let work_dir: &PathBuf = work_dir.as_ref().unwrap_or_else(|| &current_dir);

        let network = NetworkData {
            namespace: ns,
            veth: veth,
            qdisc: if tbf.is_some() || netem.is_some() {
                Some(Qdisc { tbf, netem })
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
        self.instances.add(index, network, command);
        Ok(index)
    }

    pub fn deploy(&mut self) -> anyhow::Result<()> {
        for (bridge, state) in self.bridges.values_mut() {
            if let State::Pending = state {
                netlink::bridge_apply(bridge)?;
                *state = State::Deployed;
            }
        }
        for ((from, to), state) in self.connects.iter_mut() {
            if let State::Pending = state {
                shell::bridge_connnect(
                    &self.cfg.prefix,
                    &self.bridges.get(&from).unwrap().0,
                    &self.bridges.get(&to).unwrap().0,
                )?;
                *state = State::Deployed;
            }
        }

        let data = &mut self.instances;

        let network = &data.network;
        let commands = &data.commands;
        let state = &mut data.state;
        let tasks = &mut data.tasks;

        let since = std::time::Instant::now();
        let zipped = state.iter_mut().zip(network.values());

        for ((index, state), network) in zipped {
            if let State::Pending = state {
                let bridge = self
                    .bridges
                    .get(&(index / self.cfg.instances_per_bridge))
                    .unwrap();
                netlink::namespace_apply(&network.namespace)?;
                netlink::veth_apply(&network.veth, &bridge.0)?;
                if let Some(qdisc) = &network.qdisc {
                    shell::qdisc_apply(&network.veth, qdisc)?;
                }
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
            let task = run(*index, network, command, &self.errors_sender)?;
            tasks.insert(*index, task);
            *state = State::Running;
        }
        tracing::info!("commands started in {:?}", since.elapsed());
        Ok(())
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        let data = &mut self.instances;
        let tasks = &mut data.tasks;

        let since = std::time::Instant::now();
        for task in tasks.iter_mut() {
            if let Err(err) = kill(task.1) {
                tracing::error!("failed to kill task {}: {:?}", task.0, err);
            }
        }
        for task in tasks.iter_mut() {
            if let Err(err) = wait(task.1) {
                tracing::error!("failed to stop task {}: {:?}", task.0, err);
            }
        }
        tracing::info!("commands stopped in {:?}", since.elapsed());
        tasks.clear();

        if let Some(partition) = self.partition.take() {
            partition.stop();
        }
        if self.cfg.revert {
            let since = std::time::Instant::now();
            for network in data.network.values() {
                if let Err(err) = netlink::veth_revert(&network.veth) {
                    tracing::debug!("failed to revert veth: {:?}", err);
                }   
            }
            tracing::info!("reverted veth config in {:?}", since.elapsed());
            for network in data.network.values() {
                if let Err(err) = netlink::namespace_revert(&network.namespace) {
                    tracing::debug!("failed to revert namespace: {:?}", err);
                }
            }
            tracing::info!("reverted network config in {:?}", since.elapsed());

            for ((from, to), state) in self.connects.iter_mut() {
                if let Err(err) = shell::bridge_disconnect(
                    &self.cfg.prefix,
                    &self.bridges.get(from).unwrap().0,
                    &self.bridges.get(to).unwrap().0,
                ) {
                    tracing::debug!("failed to disconnect bridges: {:?}", err);
                }
                *state = State::Pending;
            }
            for (bridge, state) in self.bridges.values_mut() {
                if let Err(err) = shell::bridge_revert(bridge) {
                    tracing::debug!("failed to revert bridge {:?}", err);
                }
                *state = State::Pending;
            }
        }
        Ok(())
    }
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
    veth: NamespaceVeth,
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
struct Instances {
    network: BTreeMap<usize, NetworkData>,
    commands: BTreeMap<usize, CommandData>,
    tasks: BTreeMap<usize, TaskData>,
    state: BTreeMap<usize, State>,
}

impl Instances {
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

fn kill(task: &mut TaskData) -> Result<()> {
    task.process.kill().context("kill process")?;
    Ok(())
}

fn wait(task: &mut TaskData) -> Result<()> {
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
