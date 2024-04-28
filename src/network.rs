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
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) addr: IpAddr,
}

impl Bridge {
    pub(crate) fn new(index: usize, prefix: &str, ip: IpAddr) -> Self {
        Bridge {
            index: index,
            name: format!("{}-b-{}", prefix, index),
            addr: ip,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct NamespaceVeth {
    pub(crate) addr: IpAddr,
    pub(crate) namespace: Namespace,
}

impl NamespaceVeth {
    pub(crate) fn new(addr: IpAddr, namespace: Namespace) -> Self {
        NamespaceVeth {
            addr,
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
