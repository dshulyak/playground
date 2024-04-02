Toolkit to test networked applications with basic chaos capabilities
---

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

- [ ] write logs from commands to selected locations instead of stdout
- [ ] spawn supervised commands that can be killed or stopped periodically
    --shutdown 'every 30m 30s pause 20m 30s' 
    every 30 minutes kill process pause for 20 minutes
    duration is counted since last event timestamp. for example 30 minutes are counted since process was started or restarted.
    pause is counted since process was stopped.
    each of this commands accept jitter as second parameter.
- [ ] partition commands from each other periodically
    --partition '50% 30% 20% every 30m duration 10s'
    --partition '1,2 3 4,5 every 30m duration 10s'
    can do it with nft by dropping traffic on the bridge that is from src => dst and dst => src
- [ ] slow/faulty disk emulation 
    need to research how it can be done