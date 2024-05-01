use std::{collections::BTreeMap, net::Ipv4Addr};

use anyhow::{ensure, Context, Result};
use ipnet::{IpAddrRange, IpNet};
use serde::{Deserialize, Serialize};

use crate::{netlink, network, shell};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum State {
    Pending,
    Deployed,
    Deleting,
    Failed,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Data {
    // vxlan may have as many entries as there are hosts
    pub(crate) vxlan: BTreeMap<usize, (network::Vxlan, State)>,
    // single bridge can be used by atmost 1000 ports (can be configured to be less)
    // number of bridges will be generated respecting per host and per bridge limits
    pub(crate) bridges: BTreeMap<usize, (network::Bridge, State)>,
    // veth pair in the namespace for every command
    pub(crate) veth: BTreeMap<usize, (network::NamespaceVeth, State)>,
    // optional netem or tbf disciplines for every command
    pub(crate) qdisc: BTreeMap<usize, (network::Qdisc, State)>,
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
}

pub struct Host {
    pub name: String,
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
    n: usize,
    hosts: &[Host],
    pool: &mut IpAddrRange,
    mut qdisc: impl Iterator<Item = (Option<String>, Option<String>)>,
) -> Result<Vec<Data>> {
    ensure!(hosts.len() > 0, "hosts must not be empty");
    hosts
        .iter()
        .enumerate()
        .map(|(index, _)| match index == 0 {
            true => n / hosts.len() + n % hosts.len(),
            false => n / hosts.len(),
        })
        .map(|n| generate_one(cfg, n, hosts, pool, &mut qdisc))
        .collect()
}

pub fn generate_one(
    cfg: &Config,
    n: usize,
    hosts: &[Host],
    pool: &mut IpAddrRange,
    mut qdisc: impl Iterator<Item = (Option<String>, Option<String>)>,
) -> Result<Data> {
    let mut data = Data::new();
    let bridges = match n % cfg.per_bridge {
        0 => n / cfg.per_bridge,
        _ => n / cfg.per_bridge + 1,
    };
    for index in 0..bridges {
        let bridge = network::Bridge::new(index, &cfg.prefix, next_addr(cfg, pool)?);
        data.bridges.insert(bridge.index, (bridge, State::Pending));
    }
    if hosts.len() > 1 {
        let vxlan = network::Vxlan {
            name: format!("vx-{}", cfg.prefix),
            id: cfg.vxlan_id,
            port: cfg.vxlan_port,
            group: cfg.vxlan_multicast_group,
            device: hosts[0].vxlan_device.clone(),
        };
        data.vxlan.insert(0, (vxlan, State::Pending));
    }
    for index in 0..n {
        let namespace = network::Namespace::new(&cfg.prefix, index);
        let veth = network::NamespaceVeth::new(next_addr(cfg, pool)?, namespace);
        data.veth.insert(index, (veth, State::Pending));
        if let Some(qdisc) = qdisc.next() {
            data.qdisc.insert(
                index,
                (
                    network::Qdisc {
                        tbf: qdisc.0,
                        netem: qdisc.1,
                    },
                    State::Pending,
                ),
            );
        }
    }
    Ok(data)
}

// deploy all tasks that are in pending state.
pub fn deploy(cfg: &Config, data: &mut Data) -> Result<()> {
    for (bridge, state) in data
        .bridges
        .values_mut()
        .filter(|(_, state)| *state == State::Pending)
    {
        netlink::bridge_apply(&bridge)?;
        *state = State::Deployed;
    }
    let first = data.bridges.values();
    let mut second = data.bridges.values();
    _ = second.next();
    for (first, second) in first.zip(second) {
        shell::bridge_connnect(&cfg.prefix, &first.0, &second.0)?;
    }
    for (vxlan, state) in data
        .vxlan
        .values_mut()
        .filter(|(_, state)| *state == State::Pending)
    {
        let bridge = data
            .bridges
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no bridges"))?;
        shell::vxlan_apply(&bridge.0, &vxlan)?;
        *state = State::Deployed;
    }
    let bridges = &data.bridges;
    for (index, (veth, state)) in data.veth.iter_mut() {
        if *state == State::Pending {
            netlink::namespace_apply(&veth.namespace)?;
            let bridge = bridges
                .get(&(index / cfg.per_bridge))
                .ok_or_else(|| anyhow::anyhow!("no bridge"))?;
            netlink::veth_apply(&veth, &bridge.0)?;
            *state = State::Deployed;
        }

        match data.qdisc.get_mut(index) {
            Some((qdisc, state)) if *state == State::Pending => {
                shell::qdisc_apply(&veth, &qdisc)?;
                *state = State::Deployed;
            }
            _ => (),
        }
    }
    Ok(())
}

// cleanup all tasks that are in deleting state.
pub fn cleanup(_: &Config, data: &mut Data) -> Result<()> {
    for (veth, _) in data.veth.values() {
        if let Err(err) = netlink::namespace_revert(&veth.namespace) {
            tracing::warn!("failed to revert namespace: {:?}", err);
        };
    }
    for (bridge, state) in data.bridges.values_mut() {
        if let Err(err) = shell::bridge_revert(&bridge) {
            tracing::warn!("failed to revert bridge: {:?}", err);
        } else {
            *state = State::Pending;
        }
    }
    for (vxlan, state) in data.vxlan.values_mut() {
        if let Err(err) = shell::vxlan_revert(&vxlan) {
            tracing::warn!("failed to revert vxlan: {:?}", err);
        } else {
            *state = State::Pending;
        };
    }
    for (veth, state) in data.veth.values_mut() {
        if let Err(err) = netlink::veth_revert(&veth) {
            tracing::warn!("failed to revert veth: {:?}", err);
        } else {
            *state = State::Pending;
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
        }
    }

    fn hosts(n: usize) -> Vec<Host> {
        (0..n)
            .map(|i| Host {
                name: format!("host{}", i),
                vxlan_device: "eth0".to_string(),
            })
            .collect()
    }

    #[test]
    fn test_generate() {
        let cfg = test_config();
        let hosts = hosts(2);
        const N: usize = 10000;
        let data = generate(&cfg, N, &hosts, &mut cfg.net.hosts(), vec![].into_iter());
        assert!(data.is_ok());
        let data = data.unwrap();
        assert_eq!(data.len(), hosts.len());
        for instance in data {
            assert_eq!(instance.vxlan.len(), 1);
            assert_eq!(instance.bridges.len(), N / hosts.len() / cfg.per_bridge);
            assert_eq!(instance.veth.len(), N / hosts.len(), "{:?}", instance.veth);
            assert_eq!(instance.qdisc.len(), 0);
        }
    }
}
