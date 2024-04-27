use anyhow::Result;
use clap::{error::ErrorKind, Command, CommandFactory, Parser, Subcommand};
use crossbeam::{channel::unbounded, select};
use playground::{partition::Partition, Env};
use std::{path::PathBuf, str::FromStr};
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
        default_value = "10.0.0.0/24",
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
}

impl Run {
    fn unique_name(&self) -> String {
        let mut name = String::new();
        for c in self.prefix.chars() {
            if c == 'X' {
                let i = rand::random::<u8>() % 10;
                name.push_str(&i.to_string());
            } else {
                name.push(c);
            }
        }
        name
    }
}

#[derive(Debug, Parser)]
struct Cleanup {
    #[clap(
        long = "prefix",
        short = 'p',
        help = "prefix for playground environment.",
        default_value = "p-"
    )]
    prefix: String,
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

    if let Err(err) = {
        let env = Env::new()
            .with_network(opts.cidr.clone())
            .with_prefix(opts.unique_name())
            .with_revert(!opts.no_revert)
            .with_redirect(opts.redirect);
        env.run(|e| {
            let first_tbf = opts.tbf.first().map(|t| t.clone());
            let first_netem = opts.netem.first().map(|n| n.clone());
            let first_count = opts.counts.first().copied().unwrap_or(1);
            let first_work_dir = opts.work_dirs.first().map(|w| w.clone());
            for (i, cmd) in opts.commands.iter().enumerate() {
                for _ in 0..opts.counts.get(i).copied().unwrap_or(first_count) {
                    let tbf = opts
                        .tbf
                        .get(i)
                        .map_or(first_tbf.clone(), |t| Some(t.clone()));
                    let netem = opts
                        .netem
                        .get(i)
                        .map_or(first_netem.clone(), |n| Some(n.clone()));

                    let work_dir = opts
                        .work_dirs
                        .get(i)
                        .or(first_work_dir.as_ref())
                        .map(|w| w.clone());

                    let _ = match e.add_task(
                        cmd.clone(),
                        work_dir.clone(),
                        tbf,
                        netem,
                        opts.env
                            .iter()
                            .map(|e| (e.0.clone(), e.1.clone()))
                            .collect(),
                    ) {
                        Ok(id) => id,
                        Err(err) => {
                            tracing::error!("failed to run command: {:?}", err);
                            return;
                        }
                    };
                }
            }
            let since = std::time::Instant::now();
            if let Err(err) = e.deploy() {
                tracing::error!("failed to deploy playground: {:?}", err);
                return;
            }
            tracing::info!("playground deployed in {:?}", since.elapsed());
            if let Some(partition) = &opts.partition {
                if let Err(err) = e.enable_partition(partition.clone()) {
                    tracing::error!("failed to spinup partition agent network: {:?}", err);
                    return;
                }
            }
            select! {
                recv(tx) -> _ => {
                    tracing::debug!("received interrupt on the channel");
                }
                recv(e.errors()) -> err => {
                    tracing::error!("error in playground: {:?}", err);
                }
            }
        })
    } {
        cmd.error(ErrorKind::Io, format!("{:?}", err)).exit();
    }
}

fn cleanup(mut cmd: Command, opts: &Cleanup) {
    let bridges = {
        match playground::cleanup_bridges(&opts.prefix) {
            Ok(bridges) => bridges,
            Err(err) => {
                cmd.error(ErrorKind::Io, format!("{:?}", err)).exit();
            }
        }
    };
    let namespaces = {
        match playground::cleanup_namespaces(&opts.prefix) {
            Ok(namespaces) => namespaces,
            Err(err) => {
                cmd.error(ErrorKind::Io, format!("{:?}", err)).exit();
            }
        }
    };
    let veth = {
        match playground::cleanup_veth(&opts.prefix) {
            Ok(veth) => veth,
            Err(err) => {
                cmd.error(ErrorKind::Io, format!("{:?}", err)).exit();
            }
        }
    };
    tracing::info!(bridges = ?bridges, namespaces = ?namespaces, veth = ?veth, "cleanup completed");
}
