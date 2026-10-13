//! Windows services via the Service Control Manager. Listing works without admin;
//! starting, stopping and changing start type need it.

use std::time::{Duration, Instant};

use serde::Serialize;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService, EnumServicesStatusExW,
    OpenSCManagerW, OpenServiceW, QueryServiceConfig2W, QueryServiceConfigW, QueryServiceStatusEx, StartServiceW,
    SC_ENUM_PROCESS_INFO, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SC_STATUS_PROCESS_INFO,
    SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONTROL_STOP, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATE_ALL,
    SERVICE_STOP, SERVICE_WIN32,
};

const SERVICE_NO_CHANGE: u32 = 0xFFFF_FFFF;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_MORE_DATA: u32 = 234;

/// Services Windows needs to stay usable. Stopping or disabling these gets a red warning.
const CRITICAL: &[&str] = &[
    "appinfo", "appxsvc", "audioendpointbuilder", "audiosrv", "bfe", "brokerinfrastructure", "clipsvc",
    "coremessagingregistrar", "cryptsvc", "dcomlaunch", "dhcp", "dispbrokerdesktopsvc", "dnscache",
    "eventlog", "eventsystem", "gpsvc", "keyiso", "lanmanworkstation", "lsm", "mpssvc", "netprofm",
    "nlasvc", "nsi", "plugplay", "power", "profsvc", "rpceptmapper", "rpcss", "samss", "schedule",
    "securityhealthservice", "sens", "sgrmbroker", "staterepository", "systemeventsbroker",
    "timebrokersvc", "trustedinstaller", "usermanager", "vaultsvc", "wcmsvc", "windefend", "winmgmt",
    "wscsvc",
];

// Mirrors of the Win32 structs we read out of raw buffers.
#[repr(C)]
struct EnumItem {
    name: *const u16,
    display: *const u16,
    service_type: u32,
    state: u32,
    controls: u32,
    exit_code: u32,
    specific_exit: u32,
    checkpoint: u32,
    wait_hint: u32,
    pid: u32,
    flags: u32,
}

#[repr(C)]
struct Config {
    service_type: u32,
    start_type: u32,
    error_control: u32,
    binary_path: *const u16,
    load_order_group: *const u16,
    tag_id: u32,
    dependencies: *const u16,
    start_name: *const u16,
    display_name: *const u16,
}

#[repr(C)]
struct StatusProcess {
    service_type: u32,
    state: u32,
    controls: u32,
    exit_code: u32,
    specific_exit: u32,
    checkpoint: u32,
    wait_hint: u32,
    pid: u32,
    flags: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Service {
    name: String,
    display: String,
    description: String,
    state: &'static str,
    start_type: &'static str,
    pid: u32,
    path: String,
    account: String,
    can_stop: bool,
    windows: bool,
    critical: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe fn wstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

fn state_name(s: u32) -> &'static str {
    match s {
        1 => "stopped", 2 => "starting", 3 => "stopping", 4 => "running",
        5 => "resuming", 6 => "pausing", 7 => "paused", _ => "unknown",
    }
}

fn start_name(t: u32, delayed: bool) -> &'static str {
    match t {
        0 => "boot", 1 => "system",
        2 if delayed => "delayed", 2 => "auto",
        3 => "manual", 4 => "disabled", _ => "unknown",
    }
}

fn last_error(action: &str) -> String {
    match unsafe { GetLastError() } {
        5 => format!("Access denied. {action} needs admin; restart Overseer as admin."),
        1051 => "Other running services depend on this one. Stop those first.".into(),
        1052 => "This service doesn't accept that command right now.".into(),
        1056 => "It's already running.".into(),
        1058 => "It's disabled. Change its startup type first.".into(),
        1060 => "That service doesn't exist anymore.".into(),
        1062 => "It isn't running.".into(),
        e => format!("{action} failed (Windows error {e})."),
    }
}

/// RAII wrapper so every handle gets closed.
struct Handle(*mut core::ffi::c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseServiceHandle(self.0 as _) };
        }
    }
}

fn manager(access: u32) -> Result<Handle, String> {
    let h = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
    if h.is_null() { Err(last_error("Opening the service manager")) } else { Ok(Handle(h as _)) }
}

fn open(scm: &Handle, name: &str, access: u32, action: &str) -> Result<Handle, String> {
    let n = wide(name);
    let h = unsafe { OpenServiceW(scm.0 as _, n.as_ptr(), access) };
    if h.is_null() { Err(last_error(action)) } else { Ok(Handle(h as _)) }
}

/// Calls a "query into a buffer" API twice: once for the size, once for real.
unsafe fn query_buf(call: impl Fn(*mut u8, u32, *mut u32) -> i32) -> Option<Vec<u64>> {
    let mut needed = 0u32;
    if call(std::ptr::null_mut(), 0, &mut needed) == 0 {
        let e = GetLastError();
        if e != ERROR_INSUFFICIENT_BUFFER && e != ERROR_MORE_DATA {
            return None;
        }
    }
    let mut buf = vec![0u64; needed as usize / 8 + 1];
    (call(buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut needed) != 0).then_some(buf)
}

fn status(svc: &Handle) -> Option<StatusProcess> {
    unsafe {
        let mut sp: StatusProcess = std::mem::zeroed();
        let mut needed = 0u32;
        let ok = QueryServiceStatusEx(
            svc.0 as _,
            SC_STATUS_PROCESS_INFO,
            &mut sp as *mut _ as *mut u8,
            std::mem::size_of::<StatusProcess>() as u32,
            &mut needed,
        );
        (ok != 0).then_some(sp)
    }
}

pub fn list() -> Result<Vec<Service>, String> {
    let scm = manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()).to_lowercase();
    let mut out = Vec::new();
    unsafe {
        let mut needed = 0u32;
        let mut count = 0u32;
        let mut resume = 0u32;
        // First call sizes the buffer.
        EnumServicesStatusExW(scm.0 as _, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL,
            std::ptr::null_mut(), 0, &mut needed, &mut count, &mut resume, std::ptr::null());
        let mut buf = vec![0u64; needed as usize / 8 + 1];
        resume = 0;
        if EnumServicesStatusExW(scm.0 as _, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL,
            buf.as_mut_ptr() as *mut u8, (buf.len() * 8) as u32, &mut needed, &mut count, &mut resume,
            std::ptr::null()) == 0
        {
            return Err(last_error("Listing services"));
        }
        let items = buf.as_ptr() as *const EnumItem;
        for i in 0..count as usize {
            let it = &*items.add(i);
            let name = wstr(it.name);
            let mut start_type = 3;
            let mut path = String::new();
            let mut account = String::new();
            let mut description = String::new();
            let mut delayed = false;
            if let Ok(svc) = open(&scm, &name, SERVICE_QUERY_CONFIG, "") {
                if let Some(b) = query_buf(|p, n, need| QueryServiceConfigW(svc.0 as _, p as _, n, need)) {
                    let c = &*(b.as_ptr() as *const Config);
                    start_type = c.start_type;
                    path = wstr(c.binary_path);
                    account = wstr(c.start_name);
                }
                if let Some(b) = query_buf(|p, n, need| QueryServiceConfig2W(svc.0 as _, SERVICE_CONFIG_DESCRIPTION, p, n, need)) {
                    description = wstr(*(b.as_ptr() as *const *const u16));
                }
                if let Some(b) = query_buf(|p, n, need| QueryServiceConfig2W(svc.0 as _, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, p, n, need)) {
                    delayed = *(b.as_ptr() as *const i32) != 0;
                }
            }
            let lower_path = path.to_lowercase().trim_start_matches('"').to_string();
            out.push(Service {
                critical: CRITICAL.contains(&name.to_lowercase().as_str()),
                windows: lower_path.starts_with(&system_root),
                display: wstr(it.display),
                state: state_name(it.state),
                start_type: start_name(start_type, delayed),
                pid: it.pid,
                can_stop: it.controls & 1 != 0, // SERVICE_ACCEPT_STOP
                name, description, path, account,
            });
        }
    }
    out.sort_by_key(|s| s.display.to_lowercase());
    Ok(out)
}

fn wait_for(svc: &Handle, target: u32) -> &'static str {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let s = status(svc).map(|s| s.state).unwrap_or(0);
        if s == target || Instant::now() > deadline {
            return state_name(s);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn stop(svc: &Handle) -> Result<(), String> {
    unsafe {
        let mut st: [u32; 7] = [0; 7]; // SERVICE_STATUS
        if ControlService(svc.0 as _, SERVICE_CONTROL_STOP, st.as_mut_ptr() as *mut _) == 0 {
            return Err(last_error("Stopping a service"));
        }
    }
    if wait_for(svc, 1) == "stopped" { Ok(()) } else { Err("It didn't stop within 15 seconds.".into()) }
}

fn start(svc: &Handle) -> Result<(), String> {
    unsafe {
        if StartServiceW(svc.0 as _, 0, std::ptr::null()) == 0 {
            return Err(last_error("Starting a service"));
        }
    }
    if wait_for(svc, 4) == "running" { Ok(()) } else { Err("It didn't finish starting within 15 seconds.".into()) }
}

/// action: start | stop | restart
pub fn control(name: &str, action: &str) -> Result<(), String> {
    let scm = manager(SC_MANAGER_CONNECT)?;
    let access = SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP;
    let svc = open(&scm, name, access, "Controlling a service")?;
    match action {
        "start" => start(&svc),
        "stop" => stop(&svc),
        "restart" => {
            if status(&svc).is_some_and(|s| s.state != 1) {
                stop(&svc)?;
            }
            start(&svc)
        }
        _ => Err(format!("Unknown action '{action}'")),
    }
}

/// start_type: auto | delayed | manual | disabled
pub fn set_start_type(name: &str, start_type: &str) -> Result<(), String> {
    let t = match start_type {
        "auto" | "delayed" => 2,
        "manual" => 3,
        "disabled" => 4,
        _ => return Err(format!("Unknown startup type '{start_type}'")),
    };
    let scm = manager(SC_MANAGER_CONNECT)?;
    let svc = open(&scm, name, SERVICE_CHANGE_CONFIG | SERVICE_QUERY_CONFIG, "Changing a startup type")?;
    unsafe {
        let ok = ChangeServiceConfigW(svc.0 as _, SERVICE_NO_CHANGE, t, SERVICE_NO_CHANGE, std::ptr::null(),
            std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(),
            std::ptr::null());
        if ok == 0 {
            return Err(last_error("Changing a startup type"));
        }
        if t == 2 {
            let mut delayed: i32 = (start_type == "delayed") as i32;
            ChangeServiceConfig2W(svc.0 as _, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, &mut delayed as *mut _ as *mut _);
        }
    }
    Ok(())
}
