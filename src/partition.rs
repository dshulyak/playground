use std::{
    collections::HashSet,
    thread::{spawn, JoinHandle},
};

use anyhow::{bail, Context, Result};
use crossbeam::{channel::Sender, select};
use humantime::Duration;

use crate::{network, shell};

#[derive(Debug, Clone)]
pub struct Partition {
    buckets: Vec<f64>,
    interval: Duration,
    duration: Duration,
}

impl Partition {
    // parse 0.5 0.3 0.2 interval 30s duration 10s
    pub fn parse(s: &str) -> Result<Self> {
        tracing::debug!("parsing partition: {}", s);
        let mut buckets = Vec::new();
        let mut splitted = s.split_whitespace().into_iter();
        while let Some(token) = splitted.next() {
            if token == "interval" {
                break;
            }
            buckets.push(token.parse::<f64>().context("can't parse into f64")?);
        }
        let sum: f64 = buckets.iter().sum();
        if sum != 1.0 {
            bail!("sum of buckets must be 1.0, got {}", sum);
        }

        let interval = splitted.next().context("missing interval")?.parse()?;
        let duration = match splitted.next() {
            Some("duration") => splitted.next().context("missing duration")?.parse()?,
            _ => bail!("missing duration"),
        };
        Ok(Self {
            buckets,
            interval,
            duration,
        })
    }
}

pub(crate) struct Task {
    partition: Partition,
    instances: Vec<network::NamespaceVeth>,
    enabled: HashSet<(network::NamespaceVeth, network::NamespaceVeth)>,
}

impl Task {
    pub(crate) fn new(partition: Partition, instances: Vec<network::NamespaceVeth>) -> Self {
        Self {
            partition,
            instances,
            enabled: HashSet::new(),
        }
    }

    pub(crate) fn apply(&mut self) -> Result<()> {
        let len = self.instances.len();
        let mut buckets: Vec<Vec<network::NamespaceVeth>> = vec![];
        let mut instances = self.instances.iter();
        for bucket in self.partition.buckets.iter() {
            buckets.push(
                instances
                    .by_ref()
                    .take((*bucket * len as f64).ceil() as usize)
                    .cloned()
                    .collect(),
            );
        }
        for (i, bucket) in buckets.iter().enumerate() {
            for to in buckets
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .flat_map(|(_, b)| b.iter())
            {
                for from in bucket {
                    shell::drop_packets_apply(from, to)?;
                    self.enabled.insert((from.clone(), to.clone()));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn revert(&mut self) -> Result<()> {
        for (from, to) in self.enabled.drain() {
            shell::drop_packets_revert(&from, &to)?;
        }
        Ok(())
    }
}

pub(crate) struct Background {
    sender: Sender<()>,
    handler: JoinHandle<()>,
}

impl Background {
    pub(crate) fn spawn(mut task: Task) -> Result<Self> {
        let (sender, receiver) = crossbeam::channel::unbounded();
        let handle = spawn(move || loop {
            select! {
                recv(receiver) -> _ => {
                    tracing::debug!("stopping partition task");
                    break;
                },
                default(task.partition.interval.into()) => {},
            }
            if let Err(err) = task.apply() {
                tracing::error!("failed to apply partition: {:?}", err);
            }
            select! {
                recv(receiver) -> _ => {
                    tracing::debug!("stopping partition task");
                    break;
                },
                default(task.partition.duration.into()) => {},
            }
            if let Err(err) = task.revert() {
                tracing::error!("failed to revert partition: {:?}", err);
            }
        });
        Ok(Self {
            sender,
            handler: handle,
        })
    }

    pub(crate) fn stop(self) {
        _ = self.sender.send(());
        self.handler.join().unwrap();
    }
}
