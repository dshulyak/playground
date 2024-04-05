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
- [ ] slow/faulty disk emulation
    not clear if i will use it.
- [ ] cgroups v2 for memory and cpu shares
    not clear if i will use it.
- [ ] distribute environment.
    very complicated. likely out of scope