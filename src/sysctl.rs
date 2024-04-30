use sysctl::Sysctl;


pub(crate) fn ensure_value(name: &str, value: &str) -> anyhow::Result<()> {
    tracing::debug!("setting sysctl {} to {}", name, value);
    let ctl = sysctl::Ctl::new(name)?;
    match ctl.value_string() {
        Ok(v) if v == value => Ok(()),
        _ => {
            ctl.set_value_string(value)?;
            Ok(())
        }
    }
}

// don't forward packets on bridge to iptables.
// https://wiki.libvirt.org/Net.bridge.bridge-nf-call_and_sysctl.conf.html
pub(crate) fn disable_bridge_nf_call_iptables() -> anyhow::Result<()> {
    ensure_value("net.bridge.bridge-nf-call-iptables", "0")
}

pub(crate) fn ipv4_neigh_gc_threash3(value: u32) -> anyhow::Result<()> {
    ensure_value("net.ipv4.neigh.default.gc_thresh3", &value.to_string())
}