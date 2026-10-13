//! Thin wrappers over the Win32 calls sysinfo doesn't cover.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    GetPriorityClass, OpenProcess, SetPriorityClass, WaitForSingleObject, ABOVE_NORMAL_PRIORITY_CLASS,
    BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SYNCHRONIZE,
    REALTIME_PRIORITY_CLASS,
};
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

pub fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

pub fn get_priority(pid: u32) -> Option<&'static str> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let class = GetPriorityClass(h);
        CloseHandle(h);
        match class {
            IDLE_PRIORITY_CLASS => Some("idle"),
            BELOW_NORMAL_PRIORITY_CLASS => Some("below"),
            NORMAL_PRIORITY_CLASS => Some("normal"),
            ABOVE_NORMAL_PRIORITY_CLASS => Some("above"),
            HIGH_PRIORITY_CLASS => Some("high"),
            REALTIME_PRIORITY_CLASS => Some("realtime"),
            _ => None,
        }
    }
}

pub fn set_priority(pid: u32, level: &str) -> Result<(), String> {
    let class = match level {
        "idle" => IDLE_PRIORITY_CLASS,
        "below" => BELOW_NORMAL_PRIORITY_CLASS,
        "normal" => NORMAL_PRIORITY_CLASS,
        "above" => ABOVE_NORMAL_PRIORITY_CLASS,
        "high" => HIGH_PRIORITY_CLASS,
        "realtime" => REALTIME_PRIORITY_CLASS,
        _ => return Err(format!("Unknown priority '{level}'")),
    };
    unsafe {
        let h = OpenProcess(PROCESS_SET_INFORMATION, 0, pid);
        if h.is_null() {
            return Err("Access denied. Try running as admin.".into());
        }
        let ok = SetPriorityClass(h, class);
        CloseHandle(h);
        if ok == 0 {
            Err("Windows rejected the priority change".into())
        } else {
            Ok(())
        }
    }
}

pub fn reveal_in_explorer(path: &Path) -> Result<(), String> {
    Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub const WAIT_FLAG: &str = "--wait-for";

/// If launched with `--wait-for <pid>`, block (up to 10s) until that process exits.
pub fn wait_for_previous_instance() {
    let args: Vec<String> = std::env::args().collect();
    let Some(pid) = args
        .iter()
        .position(|a| a == WAIT_FLAG)
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u32>().ok())
    else {
        return;
    };
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, 10_000);
            CloseHandle(h);
        }
    }
}

/// Relaunch this exe through the UAC "runas" verb.
pub fn relaunch_as_admin() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    // The new copy waits for this one to exit, so the single-instance guard lets it start.
    let params = wide(OsStr::new(&format!("{WAIT_FLAG} {}", std::process::id())));
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    if (r as isize) > 32 {
        Ok(())
    } else {
        Err("Elevation was cancelled".into())
    }
}
