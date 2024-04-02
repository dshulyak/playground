Toolkit to test networked applications with basic chaos capabilities
---

```bash
cargo build --manifest-path=./play/Cargo.toml
./target/debug/play -c "ping 10.0.0.1" -c "ping 10.0.0.2"
```

- [ ] spawn commands with qdisc
- [ ] add cleanup for bridge/namespace/veth
- [ ] spawn supervised commands that can be killed or stopped periodically
- [ ] parametrize command execution with ip and id
- [ ] write logs from commands to selected locations instead of stdout
- [ ] partition commands from each other periodically
- [ ] slow/faulty disk emulation 
    https://serverfault.com/questions/523509/linux-how-to-simulate-hard-disk-latency-i-want-to-increase-iowait-value-withou