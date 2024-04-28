use std::{
    collections::HashMap,
    net::IpAddr,
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

fn addr_to_string(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(addr) => format!("{}/24", addr),
        IpAddr::V6(addr) => format!("{}/64", addr),
    }
}

pub(crate) fn veth_apply(veth: &network::Veth, master: &network::Bridge) -> Result<()> {
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
        addr_to_string(veth.addr),
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

pub(crate) fn veth_revert(veth: &network::Veth) -> Result<()> {
    execute(&format!(
        "ip -n {} link del {}",
        veth.namespace.name,
        veth.guest()
    ))?;
    Ok(())
}

pub(crate) fn qdisc_apply(veth: &network::Veth, qdisc: &network::Qdisc) -> Result<()> {
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
    execute(&format!("ip link set {} up", bridge.name))?;
    execute(&format!(
        "ip addr add {} dev {}",
        addr_to_string(bridge.addr),
        bridge.name
    ))?;
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
    let prefix = format!("veth-{}", prefix);
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

pub(crate) fn drop_packets_apply(from: &network::Veth, to: &network::Veth) -> Result<()> {
    execute(&format!(
        "ip netns exec {} iptables -I INPUT -s {} -j DROP",
        from.namespace.name, to.addr
    ))?;
    Ok(())
}

pub(crate) fn drop_packets_revert(from: &network::Veth, to: &network::Veth) -> Result<()> {
    execute(&format!(
        "ip netns exec {} iptables -D INPUT -s {} -j DROP",
        from.namespace.name, to.addr
    ))?;
    Ok(())
}
