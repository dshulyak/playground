use std::net::IpAddr;

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
    pub(crate) name: String,
    pub(crate) addr: IpAddr,
}

impl Bridge {
    pub(crate) fn new(prefix: &str, ip: IpAddr) -> Self {
        Bridge {
            name: format!("{}-br", prefix),
            addr: ip,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Veth {
    pub(crate) addr: IpAddr,
    pub(crate) namespace: Namespace,
}

impl Veth {
    pub(crate) fn new(addr: IpAddr, namespace: Namespace) -> Self {
        Veth {
            addr,
            namespace,
        }
    }

    pub(crate) fn guest(&self) -> String {
        format!("veth-{}-ns", self.namespace.name)
    }

    pub(crate) fn host(&self) -> String {
        format!("veth-{}-br", self.namespace.name)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Qdisc {
    pub(crate) tbf: Option<String>,
    pub(crate) netem: Option<String>,
}
