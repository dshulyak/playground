use std::collections::HashMap;
use std::fmt::Display;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::str::FromStr;

use anyhow::Result;


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

trait Actionable {
    fn apply(&self) -> Result<()>;
    fn revert(&self) -> Result<()>;
}

#[derive(Debug)]
enum State {
    Pending,
    Created,
    Tombstone,
}

struct Stateful<T> {
    state: State,
    value: T,
}

type Kind = &'static str;
const TBF: Kind = "tbf";
const NETEM: Kind = "netem";

struct Env {
    prefix: String,
    next: usize,
    bridge: Stateful<Bridge>,
    namespaces: HashMap<String, Stateful<Namespace>>,
    veth: HashMap<String, Stateful<Veth>>,
    chaos: HashMap<(String, Kind), Stateful<Box<dyn Actionable>>>,
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

// man tbf
#[derive(Debug, Clone)]
struct Tbf {
    veth: Veth,
    options: String,
    parent: Option<String>,
}

impl Tbf {
    fn new(veth: Veth, options: String, parent: Option<String>) -> Self {
        Tbf {
            veth,
            options,
            parent,
        }
    }

    fn handle(&self) -> String {
        if let Some(_) = self.parent {
            String::from_str("handle 10:").expect("infallible")
        } else {
            String::from_str("handle 1:").expect("infallible")
        }
    }
}

// man netem
// netem can't be used as a parent in qdisc hierarchy
#[derive(Debug, Clone)]
struct Netem {
    veth: Veth,
    options: String,
    parent: Option<String>,
}

impl Netem {
    fn new(veth: Veth, options: String, parent: Option<String>) -> Self {
        Netem {
            veth,
            options,
            parent,
        }
    }
}