use std::{collections::BTreeMap, fmt::Display};

use serde::{Deserialize, Serialize};

use playground::{core, supervisor};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub hostname: String,
    pub vxlan_device: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Data {
    pub network: core::Data,
    pub commands: BTreeMap<usize, supervisor::CommandConfig>,
}

impl Data {
    pub fn new() -> Self {
        Data {
            network: core::Data::new(),
            commands: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum WorkerStatus {
    Pending,
    Running,
    Failed,
    Stopping,
    Stopped,
}

impl Display for WorkerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkerStatus::Pending => write!(f, "pending"),
            WorkerStatus::Running => write!(f, "running"),
            WorkerStatus::Failed => write!(f, "failed"),
            WorkerStatus::Stopping => write!(f, "stopping"),
            WorkerStatus::Stopped => write!(f, "stopped"),
        }
    }
}