use clap::{error::ErrorKind, CommandFactory, Parser};
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::str::FromStr;

#[derive(Debug, Parser)]
#[command(
    name = "playonce",
    about = "run several commands in their network namespace, introducing network latency and shaping traffic."
)]
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
        long = "tbf",
        help = "man tbf. it is passed as is to tc qdisc after tbf keyword."
    )]
    tbf: Vec<String>,
    #[clap(
        long = "netem",
        help = "man netem. it is passed as is to tc qdisc after netem keyword."
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
    if opts.commands.is_empty() {
        Opt::command()
            .error(
                ErrorKind::InvalidValue,
                "requires atleast one command to run. use --command or -c to provide commands.",
            )
            .exit();
    }
    let mut executor = ShellExecutor::new();
    if let Err(e) = run(&mut executor, &opts) {
        tracing::error!("failed to run execution: {:?}", e);
    }
    if let Err(e) = executor.revert() {
        tracing::error!("failed to revert execution: {:?}", e);
    }
}

fn run(executor: &mut ShellExecutor, opts: &Opt) -> anyhow::Result<()> {
    let mut addr = opts.cidr.hosts();
    let name = opts.unique_name();
    let bridge = Bridge::new(name.as_str());
    bridge.execute(executor)?;
    let mut runnables = vec![];
    let first_tbf = opts.tbf.first().map(|s| s.clone());
    let first_netem = opts.netem.first().map(|s| s.clone());
    for (i, cmd) in opts.commands.iter().enumerate() {
        let ns = Namespace::new(name.as_str(), i);
        let veth = Veth::new(addr.next().unwrap(), bridge.clone(), ns.clone());

        let tbf = opts
            .tbf
            .get(i)
            .map(|s| s.clone())
            .or(first_tbf.clone())
            .map(|s| Tbf::new(ns.clone(), s, None));

        let netem = opts
            .netem
            .get(i)
            .map(|s| s.clone())
            .or(first_netem.clone())
            .map(|s| Netem::new(ns.clone(), s, tbf.as_ref().map(|t| t.handle())));

        let instance = Instance::new(cmd.clone(), ns, veth, tbf, netem);
        instance.execute(executor)?;
        runnables.push(instance);
    }
    let mut childs = vec![];
    for runnable in runnables.into_iter() {
        let mut splitted = runnable.command.split_whitespace();
        let first = splitted.next().unwrap();
        let shell = Command::new(first)
            .args(splitted)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        childs.push(shell);
    }
    for mut child in childs.into_iter() {
        child.wait();
    }
    Ok(())
}

struct ShellExecutor {
    revert: Vec<String>,
}

impl ShellExecutor {
    fn new() -> Self {
        ShellExecutor { revert: vec![] }
    }

    fn execute(&mut self, cmd: &str, cleanup: Vec<&str>) -> anyhow::Result<()> {
        let mut splitted = cmd.split_whitespace();
        let first = splitted.next().unwrap();
        let shell = Command::new(first)
            .args(splitted)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let output = shell.wait_with_output()?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("{}", err));
        } else {
            self.revert.extend(cleanup.iter().map(|s| s.to_string()));
            Ok(())
        }
    }

    fn revert(&mut self) -> anyhow::Result<()> {
        for cmd in self.revert.drain(..).into_iter().rev() {
            let mut splitted = cmd.split_whitespace();
            let first = splitted.next().unwrap();
            let shell = Command::new(first)
                .args(splitted)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let output = shell.wait_with_output()?;
            if !output.status.success() {
                let err = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow::anyhow!("{}", err));
            }
        }
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

impl ShellExecutable for Bridge {
    fn execute(&self, executor: &mut ShellExecutor) -> anyhow::Result<()> {
        let cleanup = format!("ip link del {}", self.link_name());
        executor.execute(
            &format!("ip link add {} type bridge", self.link_name()),
            vec![&cleanup],
        )?;
        executor.execute(&format!("ip link set {} up", self.link_name()), vec![])?;
        Ok(())
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

impl ShellExecutable for Veth {
    fn execute(&self, executor: &mut ShellExecutor) -> anyhow::Result<()> {
        let del_veth = format!("ip link del {}", "veth1");
        let del_bridge = format!("ip link del {}", self.bridged_pair());
        executor.execute(
            &format!(
                "ip link add {} type veth peer name {}",
                "veth1",
                self.bridged_pair()
            ),
            vec![&del_veth, &del_bridge],
        )?;
        // set default namespace for veth1
        let set_default = format!("ip link set {} netns 1", "veth1");
        executor.execute(
            &format!("ip link set veth1 netns {}", self.namespace.name()),
            vec![&set_default],
        )?;
        executor.execute(
            &format!(
                "ip link set {} master {}",
                self.bridged_pair(),
                self.bridge.link_name()
            ),
            vec![],
        )?;
        executor.execute(
            &format!(
                "ip -n {} addr add {} dev veth1",
                self.namespace.name(),
                self.addr()
            ),
            vec![],
        )?;
        executor.execute(
            &format!("ip -n {} link set veth1 up", self.namespace.name()),
            vec![],
        )?;
        executor.execute(&format!("ip link set {} up", self.bridged_pair()), vec![])?;
        Ok(())
    }
}

// man tbf
#[derive(Debug, Clone)]
struct Tbf {
    namespace: Namespace,
    options: String,
    parent: Option<String>,
}

impl Tbf {
    fn new(namespace: Namespace, options: String, parent: Option<String>) -> Self {
        Tbf {
            namespace,
            options: options,
            parent,
        }
    }

    fn handle(&self) -> String {
        if let Some(_) = self.parent {
            String::from_str("handle 10:").expect("infallible")
        } else {
            String::from_str("handle 1:").expect("infallible")
        }
    }
}

impl ShellExecutable for Tbf {
    fn execute(&self, executor: &mut ShellExecutor) -> anyhow::Result<()> {
        if let Some(parent) = &self.parent {
            executor.execute(
                &format!(
                    "ip netns exec {} tc qdisc add dev veth1 parent {} handle 10: tbf {}",
                    self.namespace.name(),
                    parent,
                    self.options
                ),
                vec![],
            )?;
        } else {
            executor.execute(
                &format!(
                    "ip netns exec {} tc qdisc add dev veth1 root handle 1: tbf {}",
                    self.namespace.name(),
                    self.options
                ),
                vec![],
            )?;
        }
        Ok(())
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

impl Netem {
    fn new(namespace: Namespace, options: String, parent: Option<String>) -> Self {
        Netem {
            namespace,
            options: options,
            parent,
        }
    }
}

impl ShellExecutable for Netem {
    fn execute(&self, executor: &mut ShellExecutor) -> anyhow::Result<()> {
        if let Some(parent) = &self.parent {
            executor.execute(
                &format!(
                    "ip netns exec {} tc qdisc add dev veth1 parent {} handle 10: netem {}",
                    self.namespace.name(),
                    parent,
                    self.options
                ),
                vec![],
            )?;
        } else {
            executor.execute(
                &format!(
                    "ip netns exec {} tc qdisc add dev veth1 root handle 1: netem {}",
                    self.namespace.name(),
                    self.options
                ),
                vec![],
            )?;
        }
        Ok(())
    }
}

struct Instance {
    pub command: String,
    namespace: Namespace,
    veth: Veth,
    tbf: Option<Tbf>,
    netem: Option<Netem>,
}

impl Instance {
    fn new(
        command: String,
        namespace: Namespace,
        veth: Veth,
        tbf: Option<Tbf>,
        netem: Option<Netem>,
    ) -> Self {
        Instance {
            command,
            namespace,
            veth,
            tbf,
            netem,
        }
    }
}

impl ShellExecutable for Instance {
    fn execute(&self, executor: &mut ShellExecutor) -> anyhow::Result<()> {
        self.namespace.execute(executor)?;
        self.veth.execute(executor)?;
        if let Some(tbf) = &self.tbf {
            tbf.execute(executor)?;
        }
        if let Some(netem) = &self.netem {
            netem.execute(executor)?;
        }
        Ok(())
    }
}
