use std::net::SocketAddr;

use anyhow::Context;
use clap::{error::ErrorKind, CommandFactory, Parser};

use futures::future::{self, join_all};
use prettytable::row;
use tracing::level_filters::LevelFilter;

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[clap(long = "socket", short = 's', help = "hosts to connect to")]
    sockets: Vec<SocketAddr>,
    #[command(subcommand)]
    command: Cmds,
}

#[derive(Debug, Parser)]
struct ExecutionOpts {
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
    #[clap(
        long = "cidr",
        short = 'c',
        default_value = "10.0.0.0/16",
        help = "every command instance will be given IP address from a cidr. 
cidr is expected to have as many addresses as th sum of all commands instances"
    )]
    cidr: ipnet::IpNet,
    #[clap(
        long = "prefix",
        short = 'p',
        help = "prefix for playground environment. every `X` in the value will be replaced by random integer.",
        default_value = "p-XX"
    )]
    prefix: String,
    #[clap(
        long = "instances-per-bridge",
        help = "number of instances per bridge.",
        default_value = "1000"
    )]
    instances_per_bridge: usize,
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
}

#[derive(Debug, clap::Subcommand)]
enum Cmds {
    Hosts {},
    Preview(ExecutionOpts),
    Generate(ExecutionOpts),
}

#[tokio::main]
async fn main() {
    tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .from_env_lossy(),
        )
        .finish();
    if let Err(e) = dispatch(&Cli::parse()).await {
        Cli::command()
            .error(ErrorKind::Io, format!("failed to dispatch: {:?}", e))
            .exit();
    }
}

async fn dispatch(opts: &Cli) -> anyhow::Result<()> {
    match &opts.command {
        Cmds::Hosts {} => {
            print_hosts(&opts.sockets).await?;
        }
        Cmds::Generate(opts) => {}
        Cmds::Preview(opts) => {}
    }
    Ok(())
}

async fn host_info(host: &SocketAddr) -> anyhow::Result<playagent::HostInfo> {
    Ok(reqwest::get(format!("http://{}/host", host))
        .await
        .context("failed to download host info")?
        .json::<playagent::HostInfo>()
        .await
        .context("failed to decode json into expected response")?)
}

async fn worker_status(host: &SocketAddr) -> anyhow::Result<playagent::WorkerStatus> {
    Ok(reqwest::get(format!("http://{}/worker/status", host))
        .await
        .context("failed to download worker status")?
        .json::<playagent::WorkerStatus>()
        .await
        .context("failed to decode json into expected response")?)
}

async fn print_hosts(hosts: &[SocketAddr]) -> anyhow::Result<()> {
    let data = hosts.iter().map(|host| async move {
        match future::join(worker_status(host), host_info(host)).await {
            (Ok(worker_status), Ok(host_info)) => Ok((worker_status, host_info)),
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e),
        }
    });
    let data = join_all(data).await;

    let mut table = prettytable::Table::new();
    table.add_row(row!["order", "socket", "status", "name", "vxlan device"]);
    for (i, (socket, result)) in hosts.iter().zip(data.iter()).enumerate() {
        match result {
            Ok((status, info)) => {
                table.add_row(row![i, socket, status, info.hostname, info.vxlan_device]);
            }
            Err(e) => {
                table.add_row(row![i, socket, "ERROR", e]);
            }
        }
    }
    table.printstd();
    Ok(())
}

async fn preview(
    hosts: &[SocketAddr],
    prefix: &str,
    net: &ipnet::IpNet,
    commands: &[String],
    n: &[usize],
    per_bridge: usize,
    vxlan_id: u32,
    vxlan_port: u16,
    vxlan_multicast_group: std::net::Ipv4Addr,
    tbf: &[String],
    netem: &[String],
) -> anyhow::Result<()> {
    let data = hosts
        .iter()
        .map(|host| async move { host_info(host).await });
    let data = join_all(data).await;

    let cfg = playground::core::Config {
        prefix: prefix.to_string(),
        net: net.clone(),
        per_bridge: 1000,
        vxlan_id: vxlan_id,
        vxlan_port: vxlan_port,
        vxlan_multicast_group: vxlan_multicast_group,
    };

    // playground::core::generate(cfg, n, hosts, pool, qdisc)

    Ok(())
}
