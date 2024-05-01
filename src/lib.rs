use core::generate_one;
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

pub mod core;
mod netlink;
mod network;
pub mod partition;
pub mod shell;
mod sysctl;

// the limit of ports enforced in the kernel is 1<<10
// https://github.com/torvalds/linux/blob/80e62bc8487b049696e67ad133c503bf7f6806f7/net/bridge/br_private.h#L28
// https://github.com/moby/moby/issues/44973#issuecomment-1543733757
//
// TODO debug why does it fail with 1023 instances
const MAX_VETH_PER_BRIDGE: usize = 1000;

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
    commands: BTreeMap<usize, CommandData>,
    tasks: BTreeMap<usize, TaskData>,
    network: core::Data,
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
            commands: BTreeMap::new(),
            tasks: BTreeMap::new(),
            network: core::Data::new(),
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
            .network
            .veth
            .values()
            .map(|veth| veth.0.clone())
            .collect();
        let task = partition::Task::new(partition, veths);
        self.partition = Some(partition::Background::spawn(task)?);
        Ok(())
    }

    pub fn generate<'a>(
        &mut self,
        n: usize,
        qdisc: impl Iterator<Item = (Option<String>, Option<String>)>,
    ) -> Result<()> {
        let cfg = core::Config {
            prefix: self.cfg.prefix.clone(),
            net: self.cfg.net.clone(),
            per_bridge: self.cfg.instances_per_bridge,
            vxlan_id: 0,
            vxlan_port: 0,
            vxlan_multicast_group: "0.0.0.0".parse().unwrap(),
        };
        let hosts = vec![core::Host {
            name: "localhost".to_string(),
            vxlan_device: "lo".to_string(),
        }];
        self.network = generate_one(&cfg, n, &hosts, &mut self.hosts, qdisc)?;
        Ok(())
    }

    pub fn add(
        &mut self,
        cmd: String,
        work_dir: Option<PathBuf>,
        os_env: HashMap<String, String>,
    ) -> anyhow::Result<usize> {
        let index = self.next;
        self.next += 1;

        let current_dir = env::current_dir().context("failed to get current directory")?;
        let work_dir: &PathBuf = work_dir.as_ref().unwrap_or_else(|| &current_dir);

        let command = CommandData {
            command: cmd,
            work_dir: work_dir.clone(),
            os_env,
            redirect: self.cfg.redirect,
        };
        self.commands.insert(index, command);
        Ok(index)
    }

    pub fn deploy(&mut self) -> anyhow::Result<()> {
        sysctl::disable_bridge_nf_call_iptables()?;
        // TODO parametrize this, it starts to be an issue with certain number of instances
        sysctl::ipv4_neigh_gc_threash3(2048000)?;
        sysctl::enable_ipv4_forwarding()?;

        let cfg = core::Config {
            prefix: self.cfg.prefix.clone(),
            net: self.cfg.net.clone(),
            per_bridge: self.cfg.instances_per_bridge,
            vxlan_id: 0,
            vxlan_port: 0,
            vxlan_multicast_group: "0.0.0.0".parse().unwrap(),
        };

        let since = std::time::Instant::now();
        core::deploy(&cfg, &mut self.network)?;
        tracing::info!("configured network in {:?}", since.elapsed());

        let tasks = &mut self.tasks;

        let since = std::time::Instant::now();
        for (index, command) in self.commands.iter() {
            if !tasks.contains_key(index) {
                tasks.insert(
                    *index,
                    run(&self.cfg.prefix, *index, command, &self.errors_sender)?,
                );
            }
        }
        tracing::info!("commands started in {:?}", since.elapsed());
        Ok(())
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        let tasks = &mut self.tasks;

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
            let cfg = core::Config {
                prefix: self.cfg.prefix.clone(),
                net: self.cfg.net.clone(),
                per_bridge: self.cfg.instances_per_bridge,
                vxlan_id: 0,
                vxlan_port: 0,
                vxlan_multicast_group: "0.0.0.0".parse().unwrap(),
            };
            let since = std::time::Instant::now();
            core::cleanup(&cfg, &mut self.network)?;
            tracing::info!("network cleaned up in {:?}", since.elapsed());
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
struct CommandData {
    command: String,
    work_dir: PathBuf,
    os_env: HashMap<String, String>,
    redirect: bool,
}

#[derive(Debug)]
struct TaskData {
    output_handlers: Vec<thread::JoinHandle<()>>,
    process: Child,
}

fn run(
    prefix: &str,
    index: usize,
    command: &CommandData,
    errors: &Sender<Result<()>>,
) -> anyhow::Result<TaskData> {
    let name = network::Namespace::name(prefix, index);
    let cmd = command.command.replace("{index}", &index.to_string());
    let cmd = format!("ip netns exec {} {}", name, cmd);

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
        let stdout = OpenOptions::new()
            .append(true)
            .create(true)
            .open(command.work_dir.join(format!("{}.stdout", name)))?;
        let stderr = OpenOptions::new()
            .append(true)
            .create(true)
            .open(command.work_dir.join(format!("{}.stderr", name)))?;
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

        let id = name.clone();
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
        let id = name.clone();
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
