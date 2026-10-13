//! "Start Overseer when I sign in".
//!
//! Normal: an HKCU Run entry (no admin needed, starts without admin rights).
//! As administrator: a Task Scheduler logon task with highest privileges, so it
//! starts elevated without a UAC prompt. Creating or removing that task needs admin.

use std::os::windows::process::CommandExt;
use std::process::Command;

use serde::Serialize;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const NAME: &str = "Overseer";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const MINIMIZED_FLAG: &str = "--minimized";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    enabled: bool,
    elevated: bool,
    exe: String,
    dev_build: bool,
}

fn exe() -> Result<String, String> {
    std::env::current_exe().map(|p| p.display().to_string()).map_err(|e| e.to_string())
}

fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("schtasks").args(args).creation_flags(CREATE_NO_WINDOW).output()
}

fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", NAME]).is_ok_and(|o| o.status.success())
}

fn run_value_exists() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(NAME))
        .is_ok()
}

pub fn status() -> Result<Status, String> {
    let exe = exe()?;
    let elevated = task_exists();
    Ok(Status {
        enabled: elevated || run_value_exists(),
        elevated,
        dev_build: exe.contains(r"\target\debug\") || exe.contains(r"\target\release\"),
        exe,
    })
}

fn set_run_value(on: bool) -> Result<(), String> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN, KEY_SET_VALUE)
        .map_err(|e| e.to_string())?;
    if on {
        key.set_value(NAME, &format!("\"{}\" {MINIMIZED_FLAG}", exe()?)).map_err(|e| e.to_string())
    } else {
        match key.delete_value(NAME) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Task XML instead of plain `schtasks /Create` flags, because the defaults would
/// kill the task after 72 hours and refuse to start it on battery.
fn create_task() -> Result<(), String> {
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").map_err(|e| e.to_string())?
    );
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts Overseer in the tray at sign-in.</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>{MINIMIZED_FLAG}</Arguments></Exec></Actions>
</Task>"#,
        user = xml_escape(&user),
        exe = xml_escape(&exe()?),
    );

    // schtasks wants the XML file in UTF-16 with a BOM.
    let path = std::env::temp_dir().join("overseer-task.xml");
    let bytes: Vec<u8> = std::iter::once(0xFEFFu16)
        .chain(xml.encode_utf16())
        .flat_map(|u| u.to_le_bytes())
        .collect();
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let out = schtasks(&["/Create", "/TN", NAME, "/XML", &path.display().to_string(), "/F"]);
    let _ = std::fs::remove_file(&path);
    let out = out.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("Task Scheduler refused: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn delete_task() -> Result<(), String> {
    if !task_exists() {
        return Ok(());
    }
    let out = schtasks(&["/Delete", "/TN", NAME, "/F"]).map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err("Removing the administrator startup task needs admin. Restart Overseer as admin first.".into())
    }
}

pub fn set(enabled: bool, elevated: bool) -> Result<(), String> {
    let needs_admin = (enabled && elevated) || task_exists();
    if needs_admin && !crate::win::is_admin() {
        return Err("Changing the administrator startup task needs admin. Restart Overseer as admin first.".into());
    }
    if enabled && elevated {
        create_task()?;
        set_run_value(false)
    } else {
        delete_task()?;
        set_run_value(enabled)
    }
}
