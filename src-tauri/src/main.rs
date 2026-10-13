// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod win;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use sysinfo::{Networks, Pid, System};
use tauri::State;

struct Monitor {
    sys: System,
    nets: Networks,
    last: Instant,
}

struct AppState(Mutex<Monitor>);

/// Works whether sysinfo hands back `&str`/`String` or `&OsStr`/`OsString`.
fn lossy<S: AsRef<OsStr> + ?Sized>(s: &S) -> String {
    s.as_ref().to_string_lossy().into_owned()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Proc {
    pid: u32,
    parent: Option<u32>,
    name: String,
    cpu: f32,
    mem: u64,
    disk_read: f64,
    disk_write: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    cpu: f32,
    cores: Vec<f32>,
    mem_used: u64,
    mem_total: u64,
    swap_used: u64,
    swap_total: u64,
    net_rx: f64,
    net_tx: f64,
    disk_read: f64,
    disk_write: f64,
    uptime: u64,
    procs: Vec<Proc>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SysInfo {
    host: String,
    os: String,
    cpu_brand: String,
    cores: usize,
    admin: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Details {
    pid: u32,
    name: String,
    exe: Option<String>,
    cmd: String,
    cwd: Option<String>,
    parent: Option<u32>,
    parent_name: Option<String>,
    status: String,
    start_time: u64,
    run_time: u64,
    virt: u64,
    priority: Option<&'static str>,
}

/// Parent PID, but only if that parent is genuinely older than the child.
/// Windows reuses PIDs, so a stale parent PID can point at an unrelated process.
fn real_parent(sys: &System, p: &sysinfo::Process) -> Option<Pid> {
    let pp = p.parent()?;
    let parent = sys.process(pp)?;
    (parent.start_time() <= p.start_time()).then_some(pp)
}

#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> Result<Snapshot, String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    let m = &mut *guard;
    m.sys.refresh_all();
    m.nets.refresh();

    let now = Instant::now();
    let secs = now.duration_since(m.last).as_secs_f64().max(0.001);
    m.last = now;

    let sys = &m.sys;
    let ncpu = sys.cpus().len().max(1) as f32;

    let (mut rx, mut tx) = (0u64, 0u64);
    for d in m.nets.list().values() {
        rx += d.received();
        tx += d.transmitted();
    }

    let procs: Vec<Proc> = sys
        .processes()
        .values()
        .filter(|p| p.pid().as_u32() != 0) // "System Idle Process"
        .map(|p| {
            let d = p.disk_usage();
            Proc {
                pid: p.pid().as_u32(),
                parent: real_parent(sys, p).map(|x| x.as_u32()),
                name: lossy(p.name()),
                cpu: p.cpu_usage() / ncpu, // sysinfo is per-core; normalise like Task Manager
                mem: p.memory(),
                disk_read: d.read_bytes as f64 / secs,
                disk_write: d.written_bytes as f64 / secs,
            }
        })
        .collect();

    let disk_read = procs.iter().map(|p| p.disk_read).sum();
    let disk_write = procs.iter().map(|p| p.disk_write).sum();

    Ok(Snapshot {
        cpu: sys.global_cpu_usage(),
        cores: sys.cpus().iter().map(|c| c.cpu_usage()).collect(),
        mem_used: sys.used_memory(),
        mem_total: sys.total_memory(),
        swap_used: sys.used_swap(),
        swap_total: sys.total_swap(),
        net_rx: rx as f64 / secs,
        net_tx: tx as f64 / secs,
        disk_read,
        disk_write,
        uptime: System::uptime(),
        procs,
    })
}

#[tauri::command]
async fn system_info(state: State<'_, AppState>) -> Result<SysInfo, String> {
    let m = state.0.lock().map_err(|e| e.to_string())?;
    Ok(SysInfo {
        host: System::host_name().unwrap_or_default(),
        os: System::long_os_version().unwrap_or_default(),
        cpu_brand: m.sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default(),
        cores: m.sys.cpus().len(),
        admin: win::is_admin(),
    })
}

#[tauri::command]
async fn details(state: State<'_, AppState>, pid: u32) -> Result<Details, String> {
    let m = state.0.lock().map_err(|e| e.to_string())?;
    let p = m.sys.process(Pid::from_u32(pid)).ok_or("Process has exited")?;
    let parent = real_parent(&m.sys, p);
    Ok(Details {
        pid,
        name: lossy(p.name()),
        exe: p.exe().map(|e| e.display().to_string()),
        cmd: p.cmd().iter().map(|s| lossy(s)).collect::<Vec<_>>().join(" "),
        cwd: p.cwd().map(|c| c.display().to_string()),
        parent: parent.map(|x| x.as_u32()),
        parent_name: parent.and_then(|pp| m.sys.process(pp)).map(|pp| lossy(pp.name())),
        status: p.status().to_string(),
        start_time: p.start_time(),
        run_time: p.run_time(),
        virt: p.virtual_memory(),
        priority: win::get_priority(pid),
    })
}

/// Kill one process, or it and all its descendants (children first).
#[tauri::command]
async fn kill(state: State<'_, AppState>, pids: Vec<u32>, tree: bool) -> Result<usize, String> {
    let m = state.0.lock().map_err(|e| e.to_string())?;
    let me = std::process::id();
    let mut targets: Vec<u32> = pids.into_iter().filter(|&p| p > 4 && p != me).collect();
    if targets.is_empty() {
        return Err("Refusing to end a protected process".into());
    }

    if tree {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in m.sys.processes().values() {
            if let Some(pp) = real_parent(&m.sys, p) {
                children.entry(pp.as_u32()).or_default().push(p.pid().as_u32());
            }
        }
        let mut i = 0;
        while i < targets.len() {
            if let Some(kids) = children.get(&targets[i]) {
                for &k in kids {
                    if k != me && !targets.contains(&k) {
                        targets.push(k);
                    }
                }
            }
            i += 1;
        }
        targets.reverse();
    }

    let killed = targets
        .iter()
        .filter(|&&t| m.sys.process(Pid::from_u32(t)).is_some_and(|p| p.kill()))
        .count();

    if killed == 0 {
        Err("Couldn't end it: access denied or it already exited. Try running as admin.".into())
    } else {
        Ok(killed)
    }
}

#[tauri::command]
fn set_priority(pid: u32, level: String) -> Result<(), String> {
    win::set_priority(pid, &level)
}

#[tauri::command]
async fn open_location(state: State<'_, AppState>, pid: u32) -> Result<(), String> {
    let m = state.0.lock().map_err(|e| e.to_string())?;
    let exe = m
        .sys
        .process(Pid::from_u32(pid))
        .and_then(|p| p.exe())
        .ok_or("Path unavailable (try running as admin)")?;
    win::reveal_in_explorer(exe)
}

#[tauri::command]
fn restart_as_admin(app: tauri::AppHandle) -> Result<(), String> {
    win::relaunch_as_admin()?;
    app.exit(0);
    Ok(())
}

fn main() {
    let mut sys = System::new_all();
    sys.refresh_all();

    tauri::Builder::default()
        .manage(AppState(Mutex::new(Monitor {
            sys,
            nets: Networks::new_with_refreshed_list(),
            last: Instant::now(),
        })))
        .invoke_handler(tauri::generate_handler![
            snapshot,
            system_info,
            details,
            kill,
            set_priority,
            open_location,
            restart_as_admin
        ])
        .run(tauri::generate_context!())
        .expect("error while running Overseer");
}
