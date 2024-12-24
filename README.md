tool to bootstrap cluster of services across multiple hosts for testing. 
supports linux netem/tbf and basic network partitioning chaos utilities 
---

## How to use?

```bash
cargo install --path=./play
```

The example below will setup 2 namespace with veth pair interconnected using the same bridge.
With added delay of 100ms for outgoing packets. 

The tool supports both tbf and netem disciplines. And doesn't impose any restrictions on using them.

```bash
sudo play run -c "ping 10.0.0.3" -c "ping 10.0.0.2" --netem='delay 10ms'
```

Unless `--no-revert` is used, tool will cleanup network configuration when play run is terminated.
But in case it wasn't correctly terminated, it is possible to cleanup manually.

```bash
sudo play cleanup --prefix=<MUST BE THE SAME PREFIX AS USED IN PLAY RUN>
```

### Multiple processes

```bash
sudo play run -n 2 -c "echo first {index}" -n 3 -c "echo and then {index}"
```

Will spawn 2 process with first command and then 3 processes with second command.
There is no ordering guarantee.

### Local host reachability

Local host is available will be available on first ip in the subnet, by default 10.0.0.1.
It can be used to setup and report observability data on that host.

### Multihost setup

If workload doesn't fit on the single host, it is possible to setup multiple hosts interconnected with vxlan tunnel.

On host 1:
```bash
sudo play run -c "ping -q 10.0.0.2" -n 100 -p pi --vxlan-device eth1 -h 1/2
```

On host 2:
```bash
sudo play run -c "ping -q 10.0.0.2" -n 100 -p pi --vxlan-device eth1 -h 2/2 
```

The important difference is in `-h` flag. It is used to partition network subnet between hosts..

### Sysctl modifications

When run tool will modify the following sysctl options.

- arp cache threshing, can be diagnosed by looking at dmesg
```
sudo sysctl -w net.ipv4.neigh.default.gc_thresh3=204800
```
- docker interfering with other bridges
```
sudo sysctl -w net.bridge.bridge-nf-call-iptables=0
```

More details in https://serverfault.com/questions/963759/docker-breaks-libvirt-bridge-network

### Debugging

#### no such sysctl: net.bridge.bridge-nf-call-iptables

```bash
modprobe br_netfilter
```