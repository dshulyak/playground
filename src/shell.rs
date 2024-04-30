#![allow(dead_code)]

use std::{
    collections::HashMap,
    process::{Command, Stdio},
};

use anyhow::Result;
use serde_json::Value;

use crate::network;

fn execute(cmd: &str) -> Result<Vec<u8>> {
    tracing::debug!("running: {}", cmd);
    let mut parts = cmd.split_whitespace();
    let command = parts.next().unwrap().to_string();
    let args: Vec<_> = parts.map(|s| s.to_string()).collect();

    let execute = Command::new(command)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let output = execute.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "{}. stderr: {}",
            cmd,
            String::from_utf8(output.stderr).expect("invalid utf8")
        )
    }

    Ok(output.stdout)
}

pub(crate) fn veth_apply(veth: &network::NamespaceVeth, master: &network::Bridge) -> Result<()> {
    execute(&format!(
        "ip link add {} type veth peer name {}",
        veth.guest(),
        veth.host()
    ))?;
    execute(&format!(
        "ip link set {} netns {}",
        veth.guest(),
        veth.namespace.name
    ))?;
    execute(&format!(
        "ip link set {} master {}",
        veth.host(),
        master.name
    ))?;
    execute(&format!(
        "ip -n {} addr add {} dev {}",
        veth.namespace.name,
        veth.addr.to_string(),
        veth.guest()
    ))?;
    execute(&format!(
        "ip -n {} link set {} up",
        veth.namespace.name,
        veth.guest()
    ))?;
    execute(&format!("ip link set {} up", veth.host()))?;
    Ok(())
}

pub(crate) fn veth_revert(veth: &network::NamespaceVeth) -> Result<()> {
    execute(&format!("ip link del {}", veth.host()))?;
    execute(&format!(
        "ip -n {} link del {}",
        veth.namespace.name,
        veth.guest()
    ))?;
    Ok(())
}

pub(crate) fn qdisc_apply(veth: &network::NamespaceVeth, qdisc: &network::Qdisc) -> Result<()> {
    if let Some(tbf) = &qdisc.tbf {
        execute(&format!(
            "ip netns exec {} tc qdisc add dev {} root handle 1: tbf {}",
            veth.namespace.name,
            veth.guest(),
            tbf
        ))?;
    }
    if let Some(netem) = &qdisc.netem {
        let handle = match qdisc.tbf {
            None => "root handle 1",
            Some(_) => "parent 1:1 handle 10",
        };
        execute(&format!(
            "ip netns exec {} tc qdisc add dev {} {}: netem {}",
            veth.namespace.name,
            veth.guest(),
            handle,
            netem
        ))?;
    }
    Ok(())
}

pub(crate) fn bridge_apply(bridge: &network::Bridge) -> Result<()> {
    execute(&format!("ip link add {} type bridge", bridge.name))?;
    execute(&format!(
        "ip addr add {} dev {}",
        bridge.addr.to_string(),
        bridge.name
    ))?;
    execute(&format!("ip link set {} up", bridge.name))?;
    Ok(())
}

pub(crate) fn bridge_revert(bridge: &network::Bridge) -> Result<()> {
    execute(&format!("ip link del {}", bridge.name))?;
    Ok(())
}

pub(crate) fn namespace_apply(namespace: &network::Namespace) -> Result<()> {
    execute(&format!("ip netns add {}", namespace.name))?;
    execute(&format!(
        "ip netns exec {} ip link set lo up",
        namespace.name
    ))?;
    Ok(())
}

pub(crate) fn namespace_revert(namespace: &network::Namespace) -> Result<()> {
    execute(&format!("ip netns del {}", namespace.name))?;
    Ok(())
}

pub fn namespace_cleanup(prefix: &str) -> Result<usize> {
    let output = execute("ip -json netns list")?;
    let namespaces: Vec<HashMap<String, Value>> = serde_json::from_slice(&output)?;
    let mut count = 0;
    for ns in namespaces {
        match ns["name"] {
            Value::String(ref name) if name.starts_with(prefix) => {
                execute(&format!("ip netns del {}", name))?;
                count += 1;
            }
            _ => {}
        }
    }
    Ok(count)
}

pub fn bridge_cleanup(prefix: &str) -> Result<usize> {
    let output = execute("ip -json link show type bridge")?;
    let bridges: Vec<HashMap<String, Value>> = serde_json::from_slice(&output)?;
    let mut count = 0;
    for bridge in bridges {
        match &bridge["ifname"] {
            Value::String(ifname) if ifname.starts_with(prefix) => {
                execute(&format!("ip link del {}", ifname))?;
                count += 1;
            }
            _ => {}
        }
    }
    Ok(count)
}

pub fn veth_cleanup(prefix: &str) -> Result<usize> {
    let output = execute("ip -json link show type veth")?;
    let veths: Vec<HashMap<String, Value>> = serde_json::from_slice(&output)?;
    let mut count = 0;
    let prefix = format!("v-{}", prefix);
    for veth in veths {
        match &veth["ifname"] {
            Value::String(ifname) if ifname.starts_with(prefix.as_str()) => {
                execute(&format!("ip link del {}", ifname))?;
                count += 1;
            }
            _ => {}
        }
    }
    Ok(count)
}

pub(crate) fn drop_packets_apply(
    from: &network::NamespaceVeth,
    to: &network::NamespaceVeth,
) -> Result<()> {
    execute(&format!(
        "ip netns exec {} iptables -I INPUT -s {} -j DROP",
        from.namespace.name, to.addr
    ))?;
    Ok(())
}

pub(crate) fn drop_packets_revert(
    from: &network::NamespaceVeth,
    to: &network::NamespaceVeth,
) -> Result<()> {
    execute(&format!(
        "ip netns exec {} iptables -D INPUT -s {} -j DROP",
        from.namespace.name, to.addr
    ))?;
    Ok(())
}

fn veth_connect_pair(
    prefix: &str,
    first: &network::Bridge,
    second: &network::Bridge,
) -> (String, String) {
    (
        format!("v-{}-c{}{}-0", prefix, first.index, second.index),
        format!("v-{}-c{}{}-1", prefix, first.index, second.index),
    )
}

pub(crate) fn bridge_connnect(
    prefix: &str,
    first: &network::Bridge,
    second: &network::Bridge,
) -> Result<()> {
    let (pair0, pair1) = veth_connect_pair(prefix, first, second);
    execute(&format!(
        "ip link add name {} type veth peer name {}",
        pair0, pair1,
    ))?;
    execute(&format!("ip link set {} master {}", pair0, first.name))?;
    execute(&format!("ip link set {} master {}", pair1, second.name))?;
    execute(&format!("ip link set {} up", pair0))?;
    execute(&format!("ip link set {} up", pair1))?;
    Ok(())
}

pub(crate) fn bridge_disconnect(
    prefix: &str,
    first: &network::Bridge,
    second: &network::Bridge,
) -> Result<()> {
    let (pair0, pair1) = veth_connect_pair(prefix, first, second);
    execute(&format!("ip link del {}", pair0))?;
    execute(&format!("ip link del {}", pair1))?;
    Ok(())
}

pub(crate) fn vxlan_apply(bridge: &network::Bridge, vxlan: &network::Vxlan) -> Result<()> {
    execute(&format!(
        "ip link add {name} type vxlan id {id} group {group} dev {device} dstport {port}",
        name = vxlan.name,
        id = vxlan.id,
        group = vxlan.group,
        device = vxlan.device,
        port = vxlan.port,
    ))?;
    execute(&format!(
        "ip link set {name} master {bridge}",
        name = vxlan.name,
        bridge = bridge.name,
    ))?;
    execute(&format!("ip link set {name} up", name = vxlan.name,))?;
    Ok(())
}

pub(crate) fn vxlan_revert(vxlan: &network::Vxlan) -> Result<()> {
    execute(&format!("ip link del {name}", name = vxlan.name))?;
    Ok(())
}
