use std::{fmt::Display, net::IpAddr};

use ipnet::IpNet;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Addr(IpAddr);


impl Display for Addr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Addr {
    pub(crate) fn to_string_with_prefix(&self) -> String {
        format!("{}/{}", self.0, self.prefix())
    }

    pub(crate) fn prefix(&self) -> u8 {
        match self.0 {
            IpAddr::V4(_) => 24,
            IpAddr::V6(_) => 64,
        }
    }
}

impl From<IpAddr> for Addr {
    fn from(addr: IpAddr) -> Self {
        Addr(addr)
    }
}

impl Into<IpAddr> for Addr {
    fn into(self) -> IpAddr {
        self.0
    }
}

impl Into<IpNet> for Addr {
    fn into(self) -> IpNet {
        IpNet::new(self.0, self.prefix()).unwrap()
    }
}

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
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Bridge {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) addr: Addr,
}

impl Bridge {
    pub(crate) fn new(index: usize, prefix: &str, ip: IpAddr) -> Self {
        Bridge {
            index: index,
            name: format!("{}-b-{}", prefix, index),
            addr: ip.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct NamespaceVeth {
    pub(crate) addr: Addr,
    pub(crate) namespace: Namespace,
}

impl NamespaceVeth {
    pub(crate) fn new(addr: IpAddr, namespace: Namespace) -> Self {
        NamespaceVeth {
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

#[derive(Debug, Clone)]
pub(crate) struct Qdisc {
    pub(crate) tbf: Option<String>,
    pub(crate) netem: Option<String>,
}
