use anyhow::{bail, Result};
use std::fs;

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

    let mut preferred = Vec::new();
    let mut others = Vec::new();

    for name in list_interfaces()? {
        if name == "lo" {
            continue;
        }
        if name.starts_with("en") || name.starts_with("eth") {
            preferred.push(name);
        } else {
            others.push(name);
        }
    }

    preferred.sort();
    others.sort();
    Ok(preferred.into_iter().chain(others).next())
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
