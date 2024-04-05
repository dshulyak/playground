use std::{
    collections::HashSet,
    thread::{spawn, JoinHandle},
};

use anyhow::{bail, Context, Result};
use crossbeam::{channel::Sender, select};
use humantime::Duration;

use crate::network::{Drop, Veth};

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

pub(crate) struct PartitionTask {
    partition: Partition,
    instances: Vec<Veth>,
    enabled: HashSet<Drop>,
}

impl PartitionTask {
    pub(crate) fn new(partition: Partition, instances: Vec<Veth>) -> Self {
        Self {
            partition,
            instances,
            enabled: HashSet::new(),
        }
    }

    pub(crate) fn apply(&mut self) -> Result<()> {
        let len = self.instances.len();
        let mut buckets: Vec<Vec<Veth>> = vec![];
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
                    let drop = Drop::new(from.ns().clone(), to.ip_addr());
                    drop.apply()?;
                    self.enabled.insert(drop);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn revert(&mut self) -> Result<()> {
        for drop in self.enabled.drain() {
            drop.revert()?;
        }
        Ok(())
    }
}

pub(crate) struct PartitionBackground {
    sender: Sender<()>,
    handler: JoinHandle<()>,
}

impl PartitionBackground {
    pub(crate) fn spawn(mut task: PartitionTask) -> Result<Self> {
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
