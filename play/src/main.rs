use anyhow::{Context, Result};
use clap::{error::ErrorKind, Command, CommandFactory, Parser, Subcommand};
use crossbeam::{
    channel::{unbounded, Receiver},
    select,
};
use playground::{partition::Partition, Env};
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use std::{collections::BTreeMap, env, path::PathBuf, str::FromStr};
use tracing::metadata::LevelFilter;

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Run(Run),
    Cleanup(Cleanup),
}

#[derive(Debug, Parser)]
struct Run {
    #[clap(
        long = "command",
        short = 'c',
        help = "command to execute. 
occurances of {index} in command will be replaced with a command autoincrement"
    )]
    commands: Vec<String>,
    #[clap(
        long = "count",
        short = 'n',
        help = "number of command instances to run. 
if not provided each command is run once. 
if single value provided with multiple commands, each command will be run that many times.
otherwise the number of counts must match the number of commands."
    )]
    counts: Vec<usize>,
    #[clap(
        long = "tbf",
        help = "man tbf. it is passed as is to tc qdisc after tbf keyword.
EXAMPLES: 
--tbf 'rate 1mbit burst 80kbit latency 100ms'
"
    )]
    tbf: Vec<String>,
    #[clap(
        long = "netem",
        help = "man netem. it is passed as is to tc qdisc after netem keyword.
EXAMPLES:
--netem 'delay 100ms loss 2%' // fixed delay of 100ms and 2% packet loss on every interface
--netem 'delay 100ms 50ms'    // variable delay of 100ms with 50ms jitter on every interface 
"
    )]
    netem: Vec<String>,
    #[clap(
        long = "env",
        short = 'e',
        help = "environment variables to set for the command. KEY=VALUE"
    )]
    env: Vec<EnvValue>,
    #[clap(
        long = "cidr",
        default_value = "10.0.0.0/16",
        help = "every command instance will be given IP address from a cidr. 
cidr is expected to have as many addresses as th sum of all commands instances"
    )]
    cidr: ipnet::IpNet,
    #[clap(
        long = "prefix",
        short = 'p',
        help = "prefix for playground environment. every `X` in the value will be replaced by random integer.",
        default_value = "p-XXX"
    )]
    prefix: String,
    #[clap(
        long = "partition",
        help = "partition the network into several buckets.
first set of values are the buckets that must add up to 1.0.
interval defines how often partition is triggered, and the duration is for how long.
EXAMPLES:
    --partition='0.5 0.5 interval 5s duration 10s'
in the example above network is partitioned into two equal halves every 5s after it was restored.
it remains in the partitioned state for 10s and then gets restored.  
",
        value_parser = Partition::parse,
    )]
    partition: Option<Partition>,
    #[clap(
        long = "no-revert",
        help = "do not revert the changes made to the network configuration."
    )]
    no_revert: bool,
    #[clap(
        short = 'w',
        long = "work-dir",
        help = "working directory for the command."
    )]
    work_dirs: Vec<PathBuf>,
    #[clap(
        long = "redirect",
        help = "redirect stdout and stderr to work_dir/namespace.{stdout, stderr} files."
    )]
    redirect: bool,
    #[clap(
        long = "instances-per-bridge",
        help = "number of instances per bridge.",
        default_value = "1000"
    )]
    instances_per_bridge: usize,
    #[clap(
        long = "host",
        short = 'h',
        help = "host id to use for playground environment. the correct identifier is host_id/total_hosts.",
        default_value = "1/1"
    )]
    host_id: HostIdentifier,

    #[clap(
        long = "vxlan-id",
        help = "vxlan id to use for vxlan tunnelling",
        default_value = "1000"
    )]
    vxlan_id: u32,
    #[clap(
        long = "vxlan-port",
        help = "port to use for vxlan tunnelling",
        default_value = "4789"
    )]
    vxlan_port: u16,
    #[clap(
        long = "vxlan-multicast-group",
        help = "multicast group to use for vxlan tunnelling",
        default_value = "239.1.1.1"
    )]
    vxlan_multicast_group: std::net::Ipv4Addr,
    #[clap(
        long = "vxlan-device",
        help = "device to use for vxlan tunnelling",
        default_value = ""
    )]
    vxlan_device: String,
}

#[derive(Debug, Parser)]
struct Cleanup {
    #[clap(
        long = "prefix",
        short = 'p',
        help = "prefix for playground environment."
    )]
    prefix: String,
}

#[derive(Debug, Clone)]
struct HostIdentifier {
    id: usize,
    total: usize,
}

impl FromStr for HostIdentifier {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut splitted = s.splitn(2, '/');
        let id = splitted.next().map_or(Err("no id found".to_string()), Ok)?;
        let total = splitted
            .next()
            .map_or(Err("no total found".to_string()), Ok)?;
        Ok(HostIdentifier {
            id: id.parse().unwrap(),
            total: total.parse().unwrap(),
        })
    }
}

#[derive(Debug, Clone)]
struct EnvValue(String, String);

impl FromStr for EnvValue {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut splitted = s.splitn(2, '=');
        let key = splitted
            .next()
            .map_or(Err("no key found".to_string()), Ok)?;
        let value = splitted
            .next()
            .map_or(Err("no value found".to_string()), Ok)?;
        Ok(EnvValue(key.to_string(), value.to_string()))
    }
}

fn main() {
    if let Err(e) = tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            )
            .finish(),
    ) {
        Cli::command()
            .error(
                ErrorKind::Io,
                format!("failed to set global default subscriber: {:?}", e),
            )
            .exit();
    }
    match Cli::parse().command {
        Commands::Run(opts) => run(Cli::command(), &opts),
        Commands::Cleanup(opts) => cleanup(Cli::command(), &opts),
    }
}

fn run(mut cmd: Command, opts: &Run) {
    if opts.commands.is_empty() {
        cmd.error(
            ErrorKind::InvalidValue,
            "requires atleast one command to run. use --command or -c to provide commands.",
        )
        .exit();
    }

    let (rx, tx) = unbounded();
    if let Err(e) = ctrlc::set_handler(move || {
        tracing::info!("received interrupt. wait for program to cleanup");
        _ = rx.send(());
    }) {
        cmd.error(
            ErrorKind::Io,
            format!("failed to set interrupt handler: {:?}", e),
        )
        .exit();
    }

    let mut e = Env::new(
        opts.host_id.id,
        opts.host_id.total,
        replace_xxx(&opts.prefix),
        opts.cidr.clone(),
        opts.instances_per_bridge,
        !opts.no_revert,
        opts.redirect,
        opts.vxlan_id,
        opts.vxlan_port,
        opts.vxlan_multicast_group,
        opts.vxlan_device.clone(),
    );
    let err = rune(opts, &mut e, tx);
    if let Err(err) = e.clear() {
        tracing::error!("error during cleanup: {:?}", err);
    };
    if let Err(err) = err {
        cmd.error(ErrorKind::Io, format!("{:?}", err)).exit();
    }
}

fn rune(opts: &Run, e: &mut Env, tx: Receiver<()>) -> Result<()> {
    let first_tbf = opts.tbf.first().map(|t| t.clone());
    let first_netem = opts.netem.first().map(|n| n.clone());
    let first_count = opts.counts.first().copied().unwrap_or(1);
    let first_work_dir = opts.work_dirs.first().map(|w| w.clone());
    let current_dir = env::current_dir().context("failed to get current directory")?;

    let default_work_dir = first_work_dir.unwrap_or_else(|| current_dir);

    let total = opts
        .commands
        .iter()
        .enumerate()
        .map(|(i, _)| opts.counts.get(i).copied().unwrap_or(first_count))
        .sum();
    let qdisc = (0..total)
        .map(|index| {
            let tbf = opts.tbf.get(index).map(|t| t.clone()).or(first_tbf.clone());
            let netem = opts
                .netem
                .get(index)
                .map(|n| n.clone())
                .or(first_netem.clone());
            if tbf.is_some() || netem.is_some() {
                Some((tbf, netem))
            } else {
                None
            }
        })
        .scan((), |_, item| item);

    let commands = opts.commands.iter().enumerate().flat_map(|(i, cmd)| {
        let count = opts.counts.get(i).copied().unwrap_or(first_count);
        std::iter::repeat(cmd.clone()).take(count)
    });

    let work_dirs = (0..total).map(|index| {
        opts.work_dirs
            .get(index)
            .map_or_else(|| default_work_dir.clone(), |w| w.clone())
    });

    let os_env = opts
        .env
        .iter()
        .map(|EnvValue(k, v)| (k.clone(), v.clone()))
        .collect::<BTreeMap<_, _>>();
    let os_envs = std::iter::repeat(os_env).take(total);

    let since = std::time::Instant::now();
    e.generate(total, qdisc, commands, os_envs, work_dirs)?;
    tracing::info!("playground generated in {:?}", since.elapsed());

    let since = std::time::Instant::now();
    e.deploy()?;
    tracing::info!("playground deployed in {:?}", since.elapsed());
    if let Some(partition) = &opts.partition {
        e.enable_partition(partition.clone())?;
    }
    select! {
        recv(tx) -> _ => {
            tracing::debug!("received interrupt on the channel");
        }
        recv(e.errors()) -> err => {
            match err {
                Ok(err) => {
                    tracing::error!("error in playground: {:?}", err);
                }
                Err(_) => {
                    tracing::info!("playground completed successfully");
                }
            }
        }
    }
    Ok(())
}

fn cleanup(mut cmd: Command, opts: &Cleanup) {
    let bridges = {
        match playground::shell::bridge_cleanup(&opts.prefix) {
            Ok(bridges) => bridges,
            Err(err) => {
                cmd.error(ErrorKind::Io, format!("{:?}", err)).exit();
            }
        }
    };
    let namespaces = {
        match playground::shell::namespace_cleanup(&opts.prefix) {
            Ok(namespaces) => namespaces,
            Err(err) => {
                cmd.error(ErrorKind::Io, format!("{:?}", err)).exit();
            }
        }
    };
    let veth = {
        match playground::shell::veth_cleanup(&opts.prefix) {
            Ok(veth) => veth,
            Err(err) => {
                cmd.error(ErrorKind::Io, format!("{:?}", err)).exit();
            }
        }
    };
    tracing::info!(bridges = ?bridges, namespaces = ?namespaces, veth = ?veth, "cleanup completed");
}

fn replace_xxx(prefix: &str) -> String {
    let count = prefix.matches("X").count();
    prefix.replace(&"X".repeat(count), &random_alphanumeric(count))
}

fn random_alphanumeric(n: usize) -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}
