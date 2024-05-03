use core::generate_one;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{ensure, Result};
use crossbeam::channel::{unbounded, Receiver, Sender};
use ipnet::{IpAddrRange, IpNet};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};

pub mod core;
mod netlink;
mod network;
pub mod partition;
pub mod shell;
pub mod supervisor;
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
    commands: BTreeMap<usize, supervisor::CommandConfig>,
    tasks: BTreeMap<usize, supervisor::Execution>,
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
            .map(|veth| veth.clone())
            .collect();
        let task = partition::Task::new(partition, veths);
        self.partition = Some(partition::Background::spawn(task)?);
        Ok(())
    }

    pub fn generate(
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
        let host = core::Host {
            ip: "127.0.0.1:7777".parse().unwrap(),
            name: "localhost".to_string(),
            vxlan_device: "lo".to_string(),
        };
        self.network = generate_one(&cfg, n, false, &host, &mut self.hosts, qdisc)?;
        Ok(())
    }

    pub fn generate_commands(
        &mut self,
        n: usize,
        commands: impl Iterator<Item = String>,
        env: impl Iterator<Item = BTreeMap<String, String>>,
        workdir: impl Iterator<Item = PathBuf>,
    ) -> Result<()> {
        let exec_cfg = supervisor::generate(
            &self.cfg.prefix,
            self.cfg.redirect,
            n,
            commands,
            env,
            workdir,
        )?;
        ensure!(exec_cfg.len() == 1, "should generate for one host, instead got {:?}", exec_cfg.len());
        self.commands = exec_cfg[0].clone();
        Ok(())
    }

    pub fn deploy(&mut self) -> anyhow::Result<()> {
        sysctl::disable_bridge_nf_call_iptables()?;
        // TODO parametrize this, it starts to be an issue with certain number of instances
        sysctl::ipv4_neigh_gc_threash3(2048000)?;
        sysctl::enable_ipv4_forwarding()?;

        let since = std::time::Instant::now();
        core::deploy(&mut self.network)?;
        tracing::info!("configured network in {:?}", since.elapsed());

        let since = std::time::Instant::now();
        supervisor::launch(&self.commands, &mut self.tasks, &self.errors_sender)?;
        tracing::info!("commands started in {:?}", since.elapsed());
        Ok(())
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        let since = std::time::Instant::now();
        supervisor::stop(&mut self.tasks)?;
        tracing::info!("commands stopped in {:?}", since.elapsed());

        if let Some(partition) = self.partition.take() {
            partition.stop();
        }
        if self.cfg.revert {
            let since = std::time::Instant::now();
            core::cleanup(&mut self.network)?;
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
