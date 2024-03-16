Toolkit to test networked application locally with basic chaos capabilities
---

```bash
cargo build
./target/debug/playonce -c "ping 10.0.0.1" -c "ping 10.0.0.2"
```