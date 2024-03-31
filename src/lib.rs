use std::collections::HashMap;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::thread;

use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use anyhow::Result;
use ipnet::{IpAddrRange, IpNet};


trait Actionable {
    fn apply(&self) -> Result<()>;
    fn revert(&self) -> Result<()>;
}


struct Env {
    prefix: String,
    hosts: IpAddrRange,
    next: usize,
    bridge: Option<Bridge>,
    namespaces: HashMap<String, Namespace>,
    veth: HashMap<String, Veth>,
    qdisc: HashMap<String, Qdisc>,
}



impl Env {
    pub fn new() -> Self {
        Env {
            prefix: format!("p-{}", random_suffix(4)),
            hosts: IpNet::from_str("10.0.0.0/16").unwrap().hosts(),
            next: 0,
            bridge: None,
            namespaces: HashMap::new(),
            veth: HashMap::new(),
            qdisc: HashMap::new(),
        }
    } 

    pub fn inst(&mut self) -> Result<()> {
        let id = self.next;
        let ip = self.hosts.peekable().peek().map(|ip| *ip).unwrap();
        let ns = Namespace::new(&self.prefix, id);
        let veth = Veth::new(ip, self.bridge.as_ref().unwrap().clone(), ns.clone());
        Ok(())
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
        for veth in self.veth.values() {
            if let Err(err) = veth.revert() {
                tracing::debug!("failed to revert veth: {:?}", err);
            };
        }
        for namespace in self.namespaces.values() {
            if let Err(err) = namespace.revert() {
                tracing::debug!("failed to revert namespace: {:?}", err);
            };
        }
        if let Some(bridge) = self.bridge {
            if let Err(err) = bridge.revert() {
                tracing::debug!("failed to revert bridge: {:?}", err);
            };
        }
        rst
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
        shell(&format!("ip addr add {} dev {}", self.addr(), self.guest()))?;
        shell(&format!("ip link set {} up", self.guest()))?;
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
        .map(char::from).collect()
}