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

### Library


### Knowns workarounds

- arp cache threshing, can be diagnosed by looking at dmesg
```
sudo sysctl -w net.ipv4.neigh.default.gc_thresh3=204800
```
- docker interfering with other bridges
```
sudo sysctl -w net.bridge.bridge-nf-call-iptables=0
```

More details in https://serverfault.com/questions/963759/docker-breaks-libvirt-bridge-network
