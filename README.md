Toolkit to test networked applications with basic chaos capabilities
---

```bash
cargo build --manifest-path=./play/Cargo.toml
./target/debug/play -c "ping 10.0.0.1" -c "ping 10.0.0.2" --netem='delay 10ms'
```

- [ ] add cleanup for bridge/namespace/veth
- [ ] spawn supervised commands that can be killed or stopped periodically
    --shutdown 'every 30m 30s pause 20m 30s' 
    every 30 minutes kill process pause for 20 minutes
    duration is counted since last event timestamp. for example 30 minutes are counted since process was started or restarted.
    pause is counted since process was stopped.
    each of this commands accept jitter as second parameter.
- [ ] add workdir and env for process
- [ ] write logs from commands to selected locations instead of stdout
- [ ] partition commands from each other periodically
- [ ] slow/faulty disk emulation 
    https://serverfault.com/questions/523509/linux-how-to-simulate-hard-disk-latency-i-want-to-increase-iowait-value-withou