use std::{fmt::Display, net::Ipv4Addr};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Addr(IpNet);

impl Addr {
    pub(crate) fn ip4(&self) -> Ipv4Addr {
        match self.0.addr() {
            std::net::IpAddr::V4(ip) => ip,
            _ => panic!("not ipv4"),
        }
    }
}

impl Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Addr {
    pub(crate) fn to_string(&self) -> String {
        self.0.to_string()
    }
}

impl From<IpNet> for Addr {
    fn from(addr: IpNet) -> Self {
        Addr(addr)
    }
}

impl Into<IpNet> for Addr {
    fn into(self) -> IpNet {
        self.0
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Namespace {
    pub(crate) name: String,
}

impl Namespace {
    pub(crate) fn name(prefix: &str, index: usize) -> String {
        format!("{}-{}", prefix, index)
    }

    pub(crate) fn new(prefix: &str, index: usize) -> Self {
        Self {
            name: Self::name(prefix, index),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Bridge {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) addr: Addr,
}

impl Bridge {
    pub(crate) fn new(index: usize, prefix: &str, addr: IpNet) -> Self {
        Bridge {
            index: index,
            name: format!("{}b{}", prefix, index),
            addr: addr.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct NamespaceVeth {
    pub(crate) bridge: usize,
    pub(crate) addr: Addr,
    pub(crate) namespace: Namespace,
}

impl NamespaceVeth {
    pub(crate) fn new(bridge: usize, addr: IpNet, namespace: Namespace) -> Self {
        NamespaceVeth {
            bridge: bridge,
            addr: addr.into(),
            namespace,
        }
    }

    pub(crate) fn guest(&self) -> String {
        format!("v-{}-ns", self.namespace.name)
    }

    pub(crate) fn host(&self) -> String {
        format!("v-{}-br", self.namespace.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Qdisc {
    pub(crate) tbf: Option<String>,
    pub(crate) netem: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Vxlan {
    pub(crate) name: String,
    pub(crate) id: u32,
    pub(crate) port: u16,
    pub(crate) group: Ipv4Addr,
    pub(crate) device: String,
}

