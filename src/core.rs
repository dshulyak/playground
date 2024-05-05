use std::{collections::BTreeMap, net::Ipv4Addr};

use anyhow::{Context, Result};
use ipnet::{IpAddrRange, IpNet};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::{netlink, network, shell};

#[derive(Debug, Clone, Serialize, PartialEq, Deserialize)]
pub struct Data {
    // vxlan may have as many entries as there are hosts
    pub(crate) vxlan: BTreeMap<usize, network::Vxlan>,
    // single bridge can be used by atmost 1000 ports (can be configured to be less)
    // number of bridges will be generated respecting per host and per bridge limits
    pub(crate) bridges: BTreeMap<usize, network::Bridge>,
    // veth pair in the namespace for every command
    pub(crate) veth: BTreeMap<usize, network::NamespaceVeth>,
    // optional netem or tbf disciplines for every command
    pub(crate) qdisc: BTreeMap<usize, network::Qdisc>,
}

impl Data {
    pub fn new() -> Self {
        Self {
            vxlan: BTreeMap::new(),
            bridges: BTreeMap::new(),
            veth: BTreeMap::new(),
            qdisc: BTreeMap::new(),
        }
    }
}

pub struct Config {
    pub prefix: String,
    pub net: IpNet,
    pub per_bridge: usize,
    pub vxlan_id: u32,
    pub vxlan_port: u16,
    pub vxlan_multicast_group: Ipv4Addr,
    pub vxlan_device: String,
}

fn next_addr(cfg: &Config, pool: &mut IpAddrRange) -> Result<IpNet> {
    let addr = pool
        .next()
        .ok_or(anyhow::anyhow!("run out of ip addresses"))?;
    IpNet::new(addr, cfg.net.prefix_len()).context("failed to create ip network")
}

// generate extends data with n instances.
// in the process it generates all required configuration to interconnect instances
// between several hosts and bridges.
pub fn generate(
    cfg: &Config,
    total_hosts: usize,
    total_commands: usize,
    pool: &mut IpAddrRange,
    mut qdisc: impl Iterator<Item = (Option<String>, Option<String>)>,
) -> Result<Vec<Data>> {
    (0..total_commands)
        .chunks(total_commands / total_hosts)
        .into_iter()
        .map(|chunk| generate_one(cfg, chunk, pool, &mut qdisc))
        .collect()
}

pub fn generate_one(
    cfg: &Config,
    indexes: impl Iterator<Item = usize>,
    pool: &mut IpAddrRange,
    mut qdisc: impl Iterator<Item = (Option<String>, Option<String>)>,
) -> Result<Data> {
    let mut data = Data::new();
    if cfg.vxlan_device.len() > 0 {
        let vxlan = network::Vxlan {
            name: format!("vx-{}", cfg.prefix),
            id: cfg.vxlan_id,
            port: cfg.vxlan_port,
            group: cfg.vxlan_multicast_group,
            device: cfg.vxlan_device.to_string(),
        };
        data.vxlan.insert(0, vxlan);
    }
    for index in indexes {
        let bridge_index = index / cfg.per_bridge;
        if !data.bridges.contains_key(&bridge_index) {
            data.bridges.insert(
                bridge_index,
                network::Bridge::new(bridge_index, &cfg.prefix, next_addr(cfg, pool)?),
            );
        }
        data.veth.insert(
            index,
            network::NamespaceVeth::new(
                index / cfg.per_bridge,
                next_addr(cfg, pool)?,
                network::Namespace::new(&cfg.prefix, index),
            ),
        );
        if let Some(qdisc) = qdisc.next() {
            data.qdisc.insert(
                index,
                network::Qdisc {
                    tbf: qdisc.0,
                    netem: qdisc.1,
                },
            );
        }
    }
    Ok(data)
}

// deploy all tasks that are in pending state.
pub fn deploy(data: &Data) -> Result<()> {
    for bridge in data.bridges.values() {
        netlink::bridge_apply(&bridge)?;
    }
    let first = data.bridges.values();
    let mut second = data.bridges.values();
    _ = second.next();
    for (first, second) in first.zip(second) {
        shell::bridge_connnect(&first, &second)?;
    }
    for vxlan in data.vxlan.values() {
        let bridge = data
            .bridges
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no bridges"))?;
        shell::vxlan_apply(&bridge, &vxlan)?;
    }
    let bridges = &data.bridges;
    for (index, veth) in data.veth.iter() {
        netlink::namespace_apply(&veth.namespace)?;
        let bridge = bridges
            .get(&veth.bridge)
            .ok_or_else(|| anyhow::anyhow!("no bridge"))?;
        netlink::veth_apply(&veth, &bridge)?;

        match data.qdisc.get(index) {
            Some(qdisc) => {
                shell::qdisc_apply(&veth, &qdisc)?;
            }
            _ => (),
        }
    }
    Ok(())
}

// cleanup all tasks that are in deleting state.
pub fn cleanup(data: &Data) -> Result<()> {
    for veth in data.veth.values() {
        if let Err(err) = netlink::namespace_revert(&veth.namespace) {
            tracing::warn!("failed to revert namespace: {:?}", err);
        };
    }
    for bridge in data.bridges.values() {
        if let Err(err) = shell::bridge_revert(&bridge) {
            tracing::warn!("failed to revert bridge: {:?}", err);
        }
    }
    for vxlan in data.vxlan.values() {
        if let Err(err) = shell::vxlan_revert(&vxlan) {
            tracing::warn!("failed to revert vxlan: {:?}", err);
        }
    }
    for veth in data.veth.values() {
        if let Err(err) = netlink::veth_revert(&veth) {
            tracing::warn!("failed to revert veth: {:?}", err);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;

    fn test_config() -> Config {
        Config {
            prefix: "test".to_string(),
            net: "10.1.1.0/16".parse().unwrap(),
            per_bridge: 1000,
            vxlan_id: 100,
            vxlan_port: 4789,
            vxlan_multicast_group: "239.1.1.1".parse().unwrap(),
            vxlan_device: "eth0".to_string(),
        }
    }

    #[test]
    fn test_generate() {
        let cfg = test_config();
        const TOTAL_HOSTS: usize = 5;
        const TOTAL_COMMANDS: usize = 10000;
        let data = generate(
            &cfg,
            TOTAL_HOSTS,
            TOTAL_COMMANDS,
            &mut cfg.net.hosts(),
            vec![].into_iter(),
        );
        assert!(data.is_ok());
        let data = data.unwrap();
        assert_eq!(data.len(), TOTAL_HOSTS);
        for instance in data {
            assert_eq!(instance.vxlan.len(), 1);
            assert_eq!(
                instance.bridges.len(),
                TOTAL_COMMANDS / TOTAL_HOSTS / cfg.per_bridge
            );
            assert_eq!(
                instance.veth.len(),
                TOTAL_COMMANDS / TOTAL_HOSTS,
                "{:?}",
                instance.veth
            );
            assert_eq!(instance.qdisc.len(), 0);
        }
    }
}
