use anyhow::{bail, Context, Result};
use std::fs;

const IFF_MULTICAST: u32 = 0x1000;

pub fn resolve_interface_name_for_rtp(
    selection: Option<&str>,
    rtp_mtu: u32,
) -> Result<Option<String>> {
    let interface = resolve_interface_name(selection)?;
    if let Some(name) = interface.as_deref() {
        validate_interface_rtp_mtu(name, rtp_mtu)?;
    }
    Ok(interface)
}

pub fn resolve_interface_name(selection: Option<&str>) -> Result<Option<String>> {
    if let Some(explicit) = selection {
        let explicit = explicit.trim();
        if !explicit.is_empty() && explicit != "auto" {
            if !interface_exists(explicit) {
                bail!("requested interface {} does not exist", explicit);
            }
            if !interface_is_operational(explicit) {
                bail!("requested interface {} is not operational", explicit);
            }
            if explicit != "lo" && !interface_supports_multicast(explicit) {
                bail!(
                    "requested interface {} does not support multicast",
                    explicit
                );
            }
            return Ok(Some(explicit.to_string()));
        }
    }

    let mut candidates = Vec::new();

    for name in list_interfaces()? {
        if name == "lo" || !interface_is_operational(&name) || !interface_supports_multicast(&name)
        {
            continue;
        }

        let common_physical_name =
            name.starts_with("en") || name.starts_with("eth") || name.starts_with("wl");
        if common_physical_name || interface_is_physical(&name) {
            candidates.push(name);
        }
    }

    candidates.sort();
    candidates.dedup();

    choose_auto_interface(candidates).map(Some)
}

fn choose_auto_interface(candidates: Vec<String>) -> Result<String> {
    match candidates.as_slice() {
        [] => bail!(
            "no active multicast-capable physical interface detected; set network.interface explicitly when using a non-physical interface"
        ),
        [only] => Ok(only.clone()),
        _ => bail!(
            "multiple active multicast interfaces detected ({}); set network.interface explicitly",
            candidates.join(", ")
        ),
    }
}

fn list_interfaces() -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir("/sys/class/net")? {
        let entry = entry?;
        names.push(entry.file_name().to_string_lossy().to_string());
    }
    Ok(names)
}

fn validate_interface_rtp_mtu(name: &str, rtp_mtu: u32) -> Result<()> {
    let path = format!("/sys/class/net/{}/mtu", name);
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read link MTU for interface {}", name))?;
    let link_mtu: u32 = contents
        .trim()
        .parse()
        .with_context(|| format!("invalid link MTU for interface {}", name))?;

    if !rtp_mtu_fits_ipv4_link(link_mtu, rtp_mtu) {
        let max_rtp_mtu = link_mtu.saturating_sub(28);
        bail!(
            "network.rtp_mtu {} would exceed interface {} link MTU {} after IPv4/UDP overhead; use at most {}",
            rtp_mtu,
            name,
            link_mtu,
            max_rtp_mtu
        );
    }
    Ok(())
}

fn rtp_mtu_fits_ipv4_link(link_mtu: u32, rtp_mtu: u32) -> bool {
    rtp_mtu.saturating_add(28) <= link_mtu
}

fn interface_exists(name: &str) -> bool {
    fs::metadata(format!("/sys/class/net/{}", name)).is_ok()
}

fn interface_is_operational(name: &str) -> bool {
    let state =
        fs::read_to_string(format!("/sys/class/net/{}/operstate", name)).unwrap_or_default();
    matches!(state.trim(), "up" | "unknown")
}

fn interface_supports_multicast(name: &str) -> bool {
    let flags = fs::read_to_string(format!("/sys/class/net/{}/flags", name)).unwrap_or_default();
    let flags = flags.trim().trim_start_matches("0x");
    u32::from_str_radix(flags, 16)
        .map(|value| value & IFF_MULTICAST != 0)
        .unwrap_or(false)
}

fn interface_is_physical(name: &str) -> bool {
    fs::metadata(format!("/sys/class/net/{}/device", name)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn rtp_mtu_accounts_for_ipv4_udp_overhead() {
        assert!(rtp_mtu_fits_ipv4_link(1500, 1472));
        assert!(!rtp_mtu_fits_ipv4_link(1500, 1473));
        assert!(rtp_mtu_fits_ipv4_link(65536, 1200));
    }

    #[test]
    fn auto_requires_explicit_choice_when_wired_and_wifi_are_both_active() {
        assert!(choose_auto_interface(names(&["eth0", "wlan0"])).is_err());
    }

    #[test]
    fn auto_requires_explicit_choice_for_multiple_wired_interfaces() {
        assert!(choose_auto_interface(names(&["enp1s0", "eth0", "wlan0"])).is_err());
    }

    #[test]
    fn auto_uses_single_wifi_when_no_wired_interface_exists() {
        assert_eq!(choose_auto_interface(names(&["wlan0"])).unwrap(), "wlan0");
    }

    #[test]
    fn auto_requires_explicit_choice_for_multiple_wifi_interfaces() {
        assert!(choose_auto_interface(names(&["wlan0", "wlan1"])).is_err());
    }
}
