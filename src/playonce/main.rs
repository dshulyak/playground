use clap::Parser;
use std::fmt::format;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::str::FromStr;

#[derive(Debug, Parser)]
struct Opt {
    #[clap(long = "command", short = 'c', help = "command to execute")]
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
    #[clap(help = "man tbf. it is passed as is to tc qdisc after tbf keyword.")]
    tbf: Vec<String>,
    #[clap(help = "man netem. it is passed as is to tc qdisc after netem keyword.")]
    netem: Vec<String>,
    #[clap(
        long = "env",
        short = 'e',
        help = "environment variables to set for the command. KEY=VALUE"
    )]
    env: Vec<EnvValue>,
    #[clap(
        default_value = "10.0.0.0/24",
        help = "every command instance will be given IP address from a cidr. 
        cidr is expected to have as many addresses as th sum of all commands instances"
    )]
    cidr: ipnet::IpNet,

    #[clap(
        help = "prefix for playground environment. every `X` in the value will be replaced by random integer.",
        default_value = "p-XXX"
    )]
    prefix: String,
    // #[clap(help = "periodic signal to send to the command.")]
    // signal: Vec<String>,
    // #[clap(help = "periodically terminate the command, and restart it after a given delay.")]
    // terminate: Vec<String>,
    // #[clap(
    //     help = "periodically stop the command. unlike terminate, the command with be stopped with SIGSTOP, and resumed later"
    // )]
    // stop: Vec<String>,
}

impl Opt {
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
    let opts = Opt::parse();

    let mut addr = opts.cidr.hosts();
    let name = opts.unique_name();

    let mut commands = vec![];
    let bridge = Bridge::new(name.as_str());
    let ns = Namespace::new(name.as_str(), 0);

    let veth = Veth::new(addr.next().unwrap(), bridge.clone(), ns.clone());
    for cmd in veth.commands() {
        commands.push(cmd);
    }
    for cmd in commands.iter() {
        tracing::debug!("{}", cmd);
        let mut splitted = cmd.split_whitespace();
        let first = splitted.next().unwrap();
        let shell = Command::new(first)
            .args(splitted)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn failed");
        let output = shell.wait_with_output().expect("wait failed");
        let err = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            print!("{}", err);
        }
    }
}

trait Shell {
    fn commands(&self) -> Vec<String>;
}

struct ShellExecutor {
    revert: Vec<String>,
}

impl ShellExecutor {
    fn new() -> Self {
        ShellExecutor { revert: vec![] }
    }

    fn execute(&mut self, cmd: &str, cleanup: Vec<&str>) -> anyhow::Result<()> {
        self.revert.extend(cleanup.iter().map(|s| s.to_string()));
        Ok(())
    }
}

trait ShellExecutable {
    fn execute(&self, executor: &mut ShellExecutor) -> anyhow::Result<()>;
}

#[derive(Debug, Clone)]
struct Namespace {
    unique: String,
    i: usize,
}

impl Namespace {
    fn new(unique: &str, i: usize) -> Self {
        Namespace {
            unique: unique.to_string(),
            i,
        }
    }

    fn name(&self) -> String {
        format!("{}-{}", self.unique, self.i)
    }
}

impl ShellExecutable for Namespace {
    fn execute(&self, executor: &mut ShellExecutor) -> anyhow::Result<()> {
        let cleanup = format!("ip netns del {}", self.name());
        executor.execute(&format!("ip netns add {}", self.name()), vec![&cleanup])?;
        executor.execute(
            &format!("ip netns exec {} ip link set lo up", self.name()),
            vec![],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Bridge {
    name: String,
}

impl Bridge {
    fn new(name: &str) -> Self {
        Bridge {
            name: name.to_string(),
        }
    }

    fn link_name(&self) -> String {
        format!("{}-br", self.name)
    }
}

impl Shell for Bridge {
    fn commands(&self) -> Vec<String> {
        vec![
            format!("ip link add {} type bridge", self.link_name()),
            format!("ip link set {} up", self.link_name()),
        ]
    }
}

struct BridgeCleanup {
    bridge: Bridge,
}

impl Shell for BridgeCleanup {
    fn commands(&self) -> Vec<String> {
        vec![format!("ip link del {}", self.bridge.link_name())]
    }
}

#[derive(Debug, Clone)]
struct Veth {
    addr: IpAddr,
    bridge: Bridge,
    namespace: Namespace,
}

impl Veth {
    fn new(addr: IpAddr, bridge: Bridge, namespace: Namespace) -> Self {
        Veth {
            addr,
            bridge,
            namespace,
        }
    }

    fn addr(&self) -> String {
        match self.addr {
            IpAddr::V4(addr) => format!("{}/24", addr),
            IpAddr::V6(addr) => format!("{}/64", addr),
        }
    }

    fn bridged_pair(&self) -> String {
        format!("veth-{}-br", self.namespace.name())
    }
}

impl Shell for Veth {
    fn commands(&self) -> Vec<String> {
        vec![
            format!(
                "ip link add {} type veth peer name {}",
                "veth1",
                self.bridged_pair()
            ),
            format!("ip link set veth1 netns {}", self.namespace.name()),
            format!(
                "ip link set {} master {}",
                self.bridged_pair(),
                self.bridge.link_name()
            ),
            format!(
                "ip -n {} addr add {} dev veth1",
                self.namespace.name(),
                self.addr(),
            ),
            format!("ip -n {} link set veth1 up", self.namespace.name()),
            format!("ip link set {} up", self.bridged_pair()),
        ]
    }
}

struct VethCleanup {
    veth: Veth,
}

impl Shell for VethCleanup {
    fn commands(&self) -> Vec<String> {
        vec![
            format!("ip link del {}", self.veth.bridged_pair()),
            format!(
                "ip netns exec {} ip link del veth1",
                self.veth.namespace.name()
            ),
        ]
    }
}

// man tbf
#[derive(Debug, Clone)]
struct Tbf {
    namespace: Namespace,
    options: String,
    parent: Option<String>,
}

impl Shell for Tbf {
    fn commands(&self) -> Vec<String> {
        if let Some(parent) = &self.parent {
            vec![format!(
                "ip netns exec {} tc qdisc add dev veth1 parent {} handle 10: tbf {}",
                self.namespace.name(),
                parent,
                self.options
            )]
        } else {
            vec![format!(
                "ip netns exec {} tc qdisc add dev veth1 root handle 1: tbf {}",
                self.namespace.name(),
                self.options
            )]
        }
    }
}
// man netem
// netem can't be used as a parent in qdisc hierarchy
#[derive(Debug, Clone)]
struct Netem {
    namespace: Namespace,
    options: String,
    parent: Option<String>,
}

impl Shell for Netem {
    fn commands(&self) -> Vec<String> {
        if let Some(parent) = &self.parent {
            vec![format!(
                "ip netns exec {} tc qdisc add dev veth1 parent {} handle 10: netem {}",
                self.namespace.name(),
                parent,
                self.options
            )]
        } else {
            vec![format!(
                "ip netns exec {} tc qdisc add dev veth1 root handle 1: netem {}",
                self.namespace.name(),
                self.options,
            )]
        }
    }
}

struct Instance {
    command: String,
    namespace: Namespace,
    veth: Veth,
    tbf: Option<Tbf>,
    netem: Option<Netem>,
}
