use anyhow::{bail, Result};
use std::fs;

const IFF_MULTICAST: u32 = 0x1000;

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
    if candidates.is_empty() {
        bail!(
            "no active multicast-capable physical interface detected; set network.interface explicitly when using a non-physical interface"
        );
    }

    let non_wireless: Vec<&String> = candidates
        .iter()
        .filter(|name| !name.starts_with("wl"))
        .collect();
    match non_wireless.as_slice() {
        [only] => return Ok((*only).clone()),
        many if many.len() > 1 => {
            bail!(
                "multiple active wired/non-wireless multicast interfaces detected ({}); set network.interface explicitly",
                many.iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        _ => {}
    }

    match candidates.as_slice() {
        [only] => Ok(only.clone()),
        _ => bail!(
            "multiple active wireless multicast interfaces detected ({}); set network.interface explicitly",
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
    fn auto_prefers_single_wired_interface_over_wifi() {
        assert_eq!(
            choose_auto_interface(names(&["eth0", "wlan0"])).unwrap(),
            "eth0"
        );
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
