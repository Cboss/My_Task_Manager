// Hide the console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod etw;
mod net;
mod rules;
mod startup;
mod win;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use sysinfo::{Networks, Pid, System};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_notification::NotificationExt;

const SAMPLE_EVERY: Duration = Duration::from_secs(1);

struct Monitor {
    sys: System,
    nets: Networks,
    last: Instant,
}

/// Everything the background sampler and the commands share.
#[derive(Clone)]
struct Shared {
    monitor: Arc<Mutex<Monitor>>,
    latest: Arc<Mutex<Option<Snapshot>>>,
    bandwidth: etw::Counters,
    bandwidth_on: bool,
    rules: Arc<Mutex<rules::Engine>>,
}

/// Works whether sysinfo hands back `&str`/`String` or `&OsStr`/`OsString`.
fn lossy<S: AsRef<OsStr> + ?Sized>(s: &S) -> String {
    s.as_ref().to_string_lossy().into_owned()
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Proc {
    pid: u32,
    parent: Option<u32>,
    name: String,
    cpu: f32,
    mem: u64,
    disk_read: f64,
    disk_write: f64,
    net_rx: f64,
    net_tx: f64,
    conns: u32,
    listening: u32,
}

#[derive(Serialize, Clone)]
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
    bandwidth: bool,
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
    hotkey: &'static str,
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

fn sample(m: &mut Monitor, bandwidth: Option<&etw::Counters>) -> Snapshot {
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

    let bytes: HashMap<u32, (u64, u64)> = bandwidth
        .and_then(|b| b.lock().ok().map(|mut map| map.drain().collect()))
        .unwrap_or_default();
    let conns = net::counts();

    let procs: Vec<Proc> = sys
        .processes()
        .values()
        .filter(|p| p.pid().as_u32() != 0) // "System Idle Process"
        .map(|p| {
            let pid = p.pid().as_u32();
            let d = p.disk_usage();
            let c = conns.get(&pid).copied().unwrap_or_default();
            let (brx, btx) = bytes.get(&pid).copied().unwrap_or_default();
            Proc {
                pid,
                parent: real_parent(sys, p).map(|x| x.as_u32()),
                name: lossy(p.name()),
                cpu: p.cpu_usage() / ncpu, // sysinfo is per-core; normalise like Task Manager
                mem: p.memory(),
                disk_read: d.read_bytes as f64 / secs,
                disk_write: d.written_bytes as f64 / secs,
                net_rx: brx as f64 / secs,
                net_tx: btx as f64 / secs,
                conns: c.established,
                listening: c.listening,
            }
        })
        .collect();

    Snapshot {
        cpu: sys.global_cpu_usage(),
        cores: sys.cpus().iter().map(|c| c.cpu_usage()).collect(),
        mem_used: sys.used_memory(),
        mem_total: sys.total_memory(),
        swap_used: sys.used_swap(),
        swap_total: sys.total_swap(),
        net_rx: rx as f64 / secs,
        net_tx: tx as f64 / secs,
        disk_read: procs.iter().map(|p| p.disk_read).sum(),
        disk_write: procs.iter().map(|p| p.disk_write).sum(),
        uptime: System::uptime(),
        bandwidth: bandwidth.is_some(),
        procs,
    }
}

fn can_kill(pid: u32) -> bool {
    pid > 4 && pid != std::process::id()
}

/// Background loop: sample once a second, run rules, publish for the UI.
/// Runs whether or not the window is visible.
fn sampler(app: AppHandle, shared: Shared) {
    loop {
        let started = Instant::now();
        let snap = {
            let mut m = shared.monitor.lock().unwrap();
            let snap = sample(&mut m, shared.bandwidth_on.then_some(&shared.bandwidth));
            let sys = &m.sys;
            let kill = |pid: u32| can_kill(pid) && sys.process(Pid::from_u32(pid)).is_some_and(|p| p.kill());
            shared.rules.lock().unwrap().run(&snap.procs, &app, &kill);
            snap
        };
        if let Some(tray) = app.tray_by_id("main") {
            let mem = snap.mem_used as f64 / snap.mem_total.max(1) as f64 * 100.0;
            let _ = tray.set_tooltip(Some(format!("Overseer · CPU {:.0}% · Memory {:.0}%", snap.cpu, mem)));
        }
        *shared.latest.lock().unwrap() = Some(snap);
        std::thread::sleep(SAMPLE_EVERY.saturating_sub(started.elapsed()));
    }
}

// ---------- window / tray / hotkey ----------

const HOTKEY_LABEL: &str = "Ctrl+Alt+O";

fn hotkey() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyO)
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn toggle_main(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else { return };
    let front = w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) && w.is_focused().unwrap_or(false);
    if front {
        let _ = w.hide();
    } else {
        show_main(app);
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Overseer", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Overseer", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Overseer")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                toggle_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

// ---------- commands ----------

#[tauri::command]
fn snapshot(state: State<'_, Shared>) -> Option<Snapshot> {
    state.latest.lock().ok()?.clone()
}

#[tauri::command]
async fn system_info(state: State<'_, Shared>) -> Result<SysInfo, String> {
    let m = state.monitor.lock().map_err(|e| e.to_string())?;
    Ok(SysInfo {
        host: System::host_name().unwrap_or_default(),
        os: System::long_os_version().unwrap_or_default(),
        cpu_brand: m.sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default(),
        cores: m.sys.cpus().len(),
        admin: win::is_admin(),
        hotkey: HOTKEY_LABEL,
    })
}

#[tauri::command]
async fn details(state: State<'_, Shared>, pid: u32) -> Result<Details, String> {
    let m = state.monitor.lock().map_err(|e| e.to_string())?;
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
async fn kill(state: State<'_, Shared>, pids: Vec<u32>, tree: bool) -> Result<usize, String> {
    let m = state.monitor.lock().map_err(|e| e.to_string())?;
    let mut targets: Vec<u32> = pids.into_iter().filter(|&p| can_kill(p)).collect();
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
                    if can_kill(k) && !targets.contains(&k) {
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
async fn open_location(state: State<'_, Shared>, pid: u32) -> Result<(), String> {
    let m = state.monitor.lock().map_err(|e| e.to_string())?;
    let exe = m
        .sys
        .process(Pid::from_u32(pid))
        .and_then(|p| p.exe())
        .ok_or("Path unavailable (try running as admin)")?;
    win::reveal_in_explorer(exe)
}

#[tauri::command]
async fn connections(pid: u32) -> Vec<net::Conn> {
    net::for_pid(pid)
}

#[tauri::command]
async fn startup_items() -> Vec<startup::StartupItem> {
    startup::list()
}

#[tauri::command]
fn set_startup(scope: String, approved: String, name: String, enabled: bool) -> Result<(), String> {
    startup::set_enabled(&scope, &approved, &name, enabled)
}

#[tauri::command]
fn get_rules(state: State<'_, Shared>) -> Result<rules::RulesView, String> {
    Ok(state.rules.lock().map_err(|e| e.to_string())?.view())
}

#[tauri::command]
fn set_rules(state: State<'_, Shared>, rules: Vec<rules::Rule>) -> Result<(), String> {
    state.rules.lock().map_err(|e| e.to_string())?.set(rules)
}

#[tauri::command]
fn clear_rule_log(state: State<'_, Shared>) -> Result<(), String> {
    state.rules.lock().map_err(|e| e.to_string())?.clear_log();
    Ok(())
}

#[tauri::command]
async fn get_autostart() -> Result<autostart::Status, String> {
    autostart::status()
}

#[tauri::command]
async fn set_autostart(enabled: bool, elevated: bool) -> Result<autostart::Status, String> {
    autostart::set(enabled, elevated)?;
    autostart::status()
}

#[tauri::command]
fn restart_as_admin(app: AppHandle) -> Result<(), String> {
    win::relaunch_as_admin()?;
    app.exit(0);
    Ok(())
}

fn main() {
    win::wait_for_previous_instance();
    let start_hidden = std::env::args().any(|a| a == autostart::MINIMIZED_FLAG);

    let mut sys = System::new_all();
    sys.refresh_all();
    let monitor = Arc::new(Mutex::new(Monitor {
        sys,
        nets: Networks::new_with_refreshed_list(),
        last: Instant::now(),
    }));

    let app = tauri::Builder::default()
        // Must be first: a second launch just brings the running copy forward.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed && *shortcut == hotkey() {
                        toggle_main(app);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let bandwidth = etw::Counters::default();
            let bandwidth_on = win::is_admin() && etw::start(bandwidth.clone()).is_ok();
            let rules_path = app.path().app_config_dir().ok().map(|d| d.join("rules.json"));
            let shared = Shared {
                monitor: monitor.clone(),
                latest: Arc::new(Mutex::new(None)),
                bandwidth,
                bandwidth_on,
                rules: Arc::new(Mutex::new(rules::Engine::load(rules_path))),
            };
            app.manage(shared.clone());

            build_tray(app)?;
            if let Err(e) = app.global_shortcut().register(hotkey()) {
                eprintln!("couldn't register {HOTKEY_LABEL}: {e}");
            }

            // The window starts hidden (see tauri.conf.json); autostart keeps it in the tray.
            if !start_hidden {
                show_main(app.handle());
            }

            let handle = app.handle().clone();
            std::thread::spawn(move || sampler(handle, shared));
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing hides to the tray so rules keep running; quit from the tray menu.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                static TOLD: AtomicBool = AtomicBool::new(false);
                if !TOLD.swap(true, Ordering::Relaxed) {
                    let _ = window
                        .app_handle()
                        .notification()
                        .builder()
                        .title("Overseer is still running")
                        .body(format!("Rules keep working in the background. Press {HOTKEY_LABEL} or click the tray icon to reopen; right-click it to quit."))
                        .show();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            system_info,
            details,
            kill,
            set_priority,
            open_location,
            connections,
            startup_items,
            set_startup,
            get_rules,
            set_rules,
            clear_rule_log,
            get_autostart,
            set_autostart,
            restart_as_admin
        ])
        .build(tauri::generate_context!())
        .expect("error while building Overseer");

    app.run(|_, event| {
        if let tauri::RunEvent::Exit = event {
            etw::stop();
        }
    });
}
