//! Startup apps: registry Run keys + Startup folders, enabled/disabled the same
//! way Task Manager does it (the StartupApproved keys), so both tools agree.

use std::path::PathBuf;

use serde::Serialize;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, REG_BINARY};
use winreg::{RegKey, RegValue, HKEY};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN32: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupItem {
    name: String,
    command: String,
    location: &'static str,
    scope: &'static str,    // "user" | "machine"
    approved: &'static str, // StartupApproved subkey: Run | Run32 | StartupFolder
    enabled: bool,
}

fn hive(scope: &str) -> HKEY {
    if scope == "machine" { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER }
}

/// Missing entry = enabled. Otherwise an even first byte (02, 06) = enabled, odd (03, 07) = disabled.
fn is_enabled(scope: &str, approved: &str, name: &str) -> bool {
    RegKey::predef(hive(scope))
        .open_subkey(format!(r"{APPROVED}\{approved}"))
        .and_then(|k| k.get_raw_value(name))
        .map(|v| v.bytes.first().is_none_or(|b| b & 1 == 0))
        .unwrap_or(true)
}

fn from_run_key(out: &mut Vec<StartupItem>, scope: &'static str, path: &str, approved: &'static str, location: &'static str) {
    let Ok(key) = RegKey::predef(hive(scope)).open_subkey(path) else { return };
    for name in key.enum_values().filter_map(Result::ok).map(|(n, _)| n) {
        let command: String = key.get_value(&name).unwrap_or_default();
        out.push(StartupItem {
            enabled: is_enabled(scope, approved, &name),
            name, command, location, scope, approved,
        });
    }
}

fn from_folder(out: &mut Vec<StartupItem>, scope: &'static str, dir: Option<PathBuf>, location: &'static str) {
    let Some(dir) = dir else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        out.push(StartupItem {
            enabled: is_enabled(scope, "StartupFolder", &name),
            command: e.path().display().to_string(),
            name, location, scope,
            approved: "StartupFolder",
        });
    }
}

const STARTUP_SUFFIX: &str = r"Microsoft\Windows\Start Menu\Programs\Startup";

pub fn list() -> Vec<StartupItem> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let mut out = Vec::new();
    from_run_key(&mut out, "user", RUN, "Run", "Registry · current user");
    from_run_key(&mut out, "machine", RUN, "Run", "Registry · all users");
    from_run_key(&mut out, "machine", RUN32, "Run32", "Registry · all users (32-bit)");
    from_folder(&mut out, "user", env("APPDATA").map(|p| p.join(STARTUP_SUFFIX)), "Startup folder · current user");
    from_folder(&mut out, "machine", env("ProgramData").map(|p| p.join(STARTUP_SUFFIX)), "Startup folder · all users");
    out.sort_by_key(|i| i.name.to_lowercase());
    out
}

pub fn set_enabled(scope: &str, approved: &str, name: &str, enabled: bool) -> Result<(), String> {
    if !matches!(approved, "Run" | "Run32" | "StartupFolder") {
        return Err("Unknown startup location".into());
    }
    let (key, _) = RegKey::predef(hive(scope))
        .create_subkey_with_flags(format!(r"{APPROVED}\{approved}"), KEY_SET_VALUE)
        .map_err(|_| "Access denied. Items for all users need admin rights.".to_string())?;
    let mut bytes = vec![0u8; 12];
    bytes[0] = if enabled { 2 } else { 3 };
    key.set_raw_value(name, &RegValue { bytes: bytes.into(), vtype: REG_BINARY })
        .map_err(|_| "Access denied. Items for all users need admin rights.".to_string())
}
