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
- [ ] partition commands from each other periodically
    --partition 'partition=50% 30% 20% interval=30m duration=10s'
    --partition 'partition=1,2 3 4,5 interval=30m duration=10s'
    can do it with nft by dropping traffic on the bridge that is from src => dst and dst => src
- [ ] slow/faulty disk emulation
    not clear if i will use it.
- [ ] cgroups v2 for memory and cpu shares
    not clear if i will use it.
- [ ] distribute environment.
    very complicated. likely out of scope