use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{ensure, Result};
use crossbeam::channel::{unbounded, Receiver, Sender};
use ipnet::{IpAddrRange, IpNet};

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
pub const MAX_VETH_PER_BRIDGE: usize = 1000;

pub struct Env {
    host_id: usize,
    total_hosts: usize,
    prefix: String,
    net: IpNet,
    instances_per_bridge: usize,
    revert: bool,
    // redirect stdout and stderr to files in the working directories
    redirect: bool,
    vxlan_id: u32,
    vxlan_port: u16,
    vxlan_multicast_group: std::net::Ipv4Addr,
    vxlan_device: String,

    address_pool: IpAddrRange,
    commands: BTreeMap<usize, supervisor::CommandConfig>,
    tasks: BTreeMap<usize, supervisor::Execution>,
    network: Vec<core::Data>,
    errors_sender: Sender<anyhow::Result<()>>,
    errors_receiver: Receiver<anyhow::Result<()>>,
    partition: Option<partition::Background>,
}

impl Env {
    pub fn new(
        host_id: usize,
        total_hosts: usize,
        prefix: String,
        net: IpNet,
        per_bridge: usize,
        revert: bool,
        redirect: bool,
        vxlan_id: u32,
        vxlan_port: u16,
        vxlan_multicast_group: std::net::Ipv4Addr,
        vxlan_device: String,
    ) -> Self {
        let (sender, receiver) = unbounded();
        let hosts = net.hosts();
        Env {
            host_id,
            total_hosts,
            prefix,
            net,
            instances_per_bridge: per_bridge,
            revert,
            redirect,
            vxlan_id,
            vxlan_port,
            vxlan_multicast_group,
            vxlan_device,

            address_pool: hosts,
            commands: BTreeMap::new(),
            tasks: BTreeMap::new(),
            network: vec![],
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
            .iter()
            .flat_map(|data| data.veth.values())
            .map(|veth| veth.clone())
            .collect();
        let task = partition::Task::new(partition, veths);
        self.partition = Some(partition::Background::spawn(task)?);
        Ok(())
    }

    pub fn generate(
        &mut self,
        total_commands: usize,
        qdisc: impl Iterator<Item = (Option<String>, Option<String>)>,
        commands: impl Iterator<Item = String>,
        env: impl Iterator<Item = BTreeMap<String, String>>,
        workdir: impl Iterator<Item = PathBuf>,
    ) -> Result<()> {
        let network = core::generate(
            &core::Config {
                prefix: self.prefix.clone(),
                net: self.net.clone(),
                per_bridge: self.instances_per_bridge,
                vxlan_id: self.vxlan_id,
                vxlan_port: self.vxlan_port,
                vxlan_multicast_group: self.vxlan_multicast_group,
                vxlan_device: self.vxlan_device.clone(),
            },
            self.total_hosts,
            total_commands,
            &mut self.address_pool,
            qdisc,
        )?;
        let commands = supervisor::generate(
            &self.prefix,
            self.redirect,
            network.iter().map(|data| data.veth.len()),
            commands,
            env,
            workdir,
        )?;

        ensure!(
            network.len() == self.total_hosts,
            "should generate for all hosts, instead got {:?}",
            network.len()
        );
        ensure!(
            commands.len() == self.total_hosts,
            "should generate for all hosts {:?}", commands.len(),
        );

        self.network = network;
        self.commands = commands[self.host_id - 1].clone();
        Ok(())
    }

    pub fn deploy(&mut self) -> anyhow::Result<()> {
        sysctl::disable_bridge_nf_call_iptables()?;
        // TODO parametrize this, it starts to be an issue with certain number of instances
        sysctl::ipv4_neigh_gc_threash3(2048000)?;
        sysctl::enable_ipv4_forwarding()?;

        let since = std::time::Instant::now();
        core::deploy(&mut self.network[self.host_id - 1])?;
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
        if self.revert {
            let since = std::time::Instant::now();
            if let Some(data) = self.network.get(self.host_id - 1) {
                core::cleanup(data)?;
            }
            tracing::info!("network cleaned up in {:?}", since.elapsed());
        }
        Ok(())
    }
}
