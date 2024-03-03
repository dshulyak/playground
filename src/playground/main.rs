use clap::Parser;
use std::net::IpAddr;
use std::process::{Command, Stdio};

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
    #[clap(
        long = "env",
        short = 'e',
        help = "environment variables to set for the command. 
        if not provided the current environment is used.
        if single value provided with multiple commands, each command will be run with that environment.
        otherwise the number of envs must match the number of commands."
    )]
    env: Vec<String>,
    #[clap(
        default_value = "10.0.0.0/24",
        help = "every command instance will be given IP address from a cidr"
    )]
    cidr: ipnet::IpNet,
}

fn main() {
    let opts = Opt::parse();
    let mut addr = opts.cidr.hosts();

    let mut commands = vec![];
    let ns = Namespace::new("ns2");
    for cmd in ns.commands() {
        commands.push(cmd);
    }
    let bridge = Bridge::new("play0");
    for cmd in bridge.commands() {
        commands.push(cmd);
    }
    let veth = Veth::new(addr.next().unwrap(), bridge.clone(), ns.clone());
    for cmd in veth.commands() {
        commands.push(cmd);
    }
    for cmd in commands.iter() {
        println!("{}", cmd);
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

#[derive(Debug, Clone)]
struct Namespace {
    name: String,
}

impl Namespace {
    fn new(name: &str) -> Self {
        Namespace {
            name: name.to_string(),
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }
}

impl Shell for Namespace {
    fn commands(&self) -> Vec<String> {
        vec![
            format!("ip netns add {}", self.name),
            format!("ip netns exec {} ip link set lo up", self.name),
        ]
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
