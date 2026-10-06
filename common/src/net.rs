use anyhow::{bail, Result};
use std::fs;

const IFF_MULTICAST: u32 = 0x1000;

pub fn resolve_interface_name(selection: Option<&str>) -> Result<Option<String>> {
    if let Some(explicit) = selection {
        let explicit = explicit.trim();
        if !explicit.is_empty() && explicit != "auto" {
            if interface_exists(explicit) {
                return Ok(Some(explicit.to_string()));
            }
            bail!("requested interface {} does not exist", explicit);
        }
    }

    let mut candidates = Vec::new();

    for name in list_interfaces()? {
        if name == "lo" || !interface_is_operational(&name) || !interface_supports_multicast(&name) {
            continue;
        }

        let common_physical_name = name.starts_with("en")
            || name.starts_with("eth")
            || name.starts_with("wl");
        if common_physical_name || interface_is_physical(&name) {
            candidates.push(name);
        }
    }

    candidates.sort();
    candidates.dedup();

    match candidates.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.clone())),
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

fn interface_exists(name: &str) -> bool {
    fs::metadata(format!("/sys/class/net/{}", name)).is_ok()
}

fn interface_is_operational(name: &str) -> bool {
    let state = fs::read_to_string(format!("/sys/class/net/{}/operstate", name))
        .unwrap_or_default();
    matches!(state.trim(), "up" | "unknown")
}

fn interface_supports_multicast(name: &str) -> bool {
    let flags = fs::read_to_string(format!("/sys/class/net/{}/flags", name))
        .unwrap_or_default();
    let flags = flags.trim().trim_start_matches("0x");
    u32::from_str_radix(flags, 16)
        .map(|value| value & IFF_MULTICAST != 0)
        .unwrap_or(false)
}

fn interface_is_physical(name: &str) -> bool {
    fs::metadata(format!("/sys/class/net/{}/device", name)).is_ok()
}
