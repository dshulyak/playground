use std::collections::HashMap;
use std::net::IpAddr;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::Value;


#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct Namespace {
    pub(crate) name: String,
}

impl Namespace {
    pub(crate) fn new(prefix: &str, index: usize) -> Self {
        Self {
            name: format!("{}-{}", prefix, index),
        }
    }

    pub(crate) fn cleanup(prefix: &str) -> Result<usize> {
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

    pub(crate) fn apply(&self) -> Result<()> {
        shell(&format!("ip netns add {}", self.name))?;
        shell(&format!("ip netns exec {} ip link set lo up", self.name))?;
        Ok(())
    }

    pub(crate) fn revert(&self) -> Result<()> {
        shell(&format!("ip netns del {}", self.name))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Bridge {
    name: String,
    addr: IpAddr,
}

impl Bridge {
    pub(crate) fn new(ns: &str, ip: IpAddr) -> Self {
        Bridge {
            name: format!("{}-br", ns),
            addr: ip,
        }
    }

    pub(crate) fn cleanup(prefix: &str) -> Result<usize> {
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

    pub(crate) fn apply(&self) -> Result<()> {
        shell(&format!("ip link add {} type bridge", self.name))?;
        shell(&format!("ip link set {} up", self.name))?;
        shell(&format!("ip addr add {} dev {}", addr_to_string(self.addr), self.name))?;
        Ok(())
    }

    pub(crate) fn revert(&self) -> Result<()> {
        shell(&format!("ip link del {}", self.name))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Veth {
    addr: IpAddr,
    bridge: Bridge,
    namespace: Namespace,
}

impl Veth {
    pub(crate) fn new(addr: IpAddr, bridge: Bridge, namespace: Namespace) -> Self {
        Veth {
            addr,
            bridge,
            namespace,
        }
    }

    pub(crate) fn ns(&self) -> &Namespace {
        &self.namespace
    }

    pub(crate) fn ip_addr(&self) -> IpAddr {
        self.addr
    }

    fn guest(&self) -> String {
        format!("veth-{}-ns", self.namespace.name)
    }

    fn host(&self) -> String {
        format!("veth-{}-br", self.namespace.name)
    }

    pub(crate) fn cleanup(prefix: &str) -> Result<usize> {
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

    pub(crate) fn apply(&self) -> Result<()> {
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
            addr_to_string(self.addr),
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

    pub(crate) fn revert(&self) -> Result<()> {
        shell(&format!(
            "ip -n {} link del {}",
            self.namespace.name,
            self.guest()
        ))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Qdisc {
    veth: Veth,
    tbf: Option<String>,
    netem: Option<String>,
}

impl Qdisc {
    pub(crate) fn new(veth: Veth, tbf: Option<String>, netem: Option<String>) -> Self {
        Qdisc { veth, tbf, netem }
    }

    pub(crate) fn apply(&self) -> Result<()> {
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

    pub(crate) fn revert(&self) -> Result<()> {
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

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct Drop {
    ns: Namespace,
    addr: IpAddr,
}

impl Drop {
    pub(crate) fn new(ns: Namespace, addr: IpAddr) -> Self {
        Drop { ns, addr }
    }

    pub(crate) fn apply(&self) -> Result<()> {
        shell(&format!(
            "ip netns exec {} iptables -A INPUT -s {} -j DROP",
            self.ns.name,
            self.addr
        ))?;
        Ok(())
    }

    pub(crate) fn revert(&self) -> Result<()> {
        shell(&format!(
            "ip netns exec {} iptables -D INPUT -s {} -j DROP",
            self.ns.name,
            self.addr
        ))?;
        Ok(())
    }
}

fn addr_to_string(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(addr) => format!("{}/24", addr),
        IpAddr::V6(addr) => format!("{}/64", addr),
    }
}