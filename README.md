Toolkit for testing and debugging distributed applications with basic chaos capabilities.
---

## How to use?
### Command line
```bash
cargo build --manifest-path=./play/Cargo.toml
export PATH=$PATH:./target/debug/
```

```bash
play run -c "ping 10.0.0.3" -c "ping 10.0.0.2" --netem='delay 10ms'
```

```bash
play cleanup
```

### Multihost setup

playground can setup environment on multiple hosts, connected with multicast vxlan. The example is below, note that both sides need to have consistent configuration. 

The important difference is with `-h 1/2` and `-h 2/2`, this way each side will deploy it is own share of commands, with correct ips.

```bash
vagrant up
vagrant ssh first 
sudo /target/release/play run -c "ping -q 10.0.0.2" -n 100 -p pi --vxlan-device eth1 -h 1/2
# in another console
vagrant ssh second
sudo /target/release/play run -c "ping -q 10.0.0.2" -n 100 -p pi --vxlan-device eth1 -h 2/2
```

### Library


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
