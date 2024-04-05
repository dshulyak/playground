Toolkit for testing and debugging distributed applications with basic chaos capabilities.
---

## How to use?
### Command line
```bash
cargo build --manifest-path=./play/Cargo.toml
export PATH=$PATH:./target/debug/
```

```bash
play run -c "ping 10.0.0.1" -c "ping 10.0.0.2" --netem='delay 10ms'
```

```bash
play cleanup
```

### Library


## TODO
- [ ] slow/faulty disk emulation ?
- [ ] cgroups v2 for memory limits and cpu shares ?
- [ ] distributed environment ?
  i can do that with primary/agent and vxlan tunnel for network.