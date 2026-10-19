//! Kernel ETW tracing (needs administrator rights). One session hosts two providers:
//! - Microsoft-Windows-Kernel-Network: per-process bandwidth
//! - Microsoft-Windows-Kernel-File: per-process file activity (see files.rs)

use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::sync::{Arc, Mutex};

use ferrisetw::parser::{Parser, Pointer};
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::UserTrace;
use ferrisetw::EventRecord;

use crate::files;

const SESSION: &str = "Overseer";
const LEGACY_SESSIONS: &[&str] = &["Overseer-Net"]; // pre-0.4 name
const KERNEL_NETWORK: &str = "7dd42a49-5329-4832-8dfd-43d979153a88";
const KERNEL_FILE: &str = "edd08927-9cc4-4e65-b970-c2560fb5c289";
// FILENAME | CREATE | READ | WRITE. FILEIO (0x20) is left out: it adds close/flush/query noise.
const FILE_KEYWORDS: u64 = 0x10 | 0x80 | 0x100 | 0x200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Bytes (received, sent) per PID since the last drain.
pub type Counters = Arc<Mutex<HashMap<u32, (u64, u64)>>>;

static TRACE: Mutex<Option<UserTrace>> = Mutex::new(None);

fn network_provider(counters: Counters) -> Provider {
    Provider::by_guid(KERNEL_NETWORK)
        .add_callback(move |record: &EventRecord, locator: &SchemaLocator| {
            // TCP and UDP data events, IPv4 and IPv6.
            let sent = match record.event_id() {
                10 | 26 | 42 | 58 => true,
                11 | 27 | 43 | 59 => false,
                _ => return,
            };
            let Ok(schema) = locator.event_schema(record) else { return };
            let parser = Parser::create(record, &schema);
            let (Ok(pid), Ok(size)) = (parser.try_parse::<u32>("PID"), parser.try_parse::<u32>("size")) else {
                return;
            };
            if let Ok(mut map) = counters.lock() {
                let e = map.entry(pid).or_default();
                if sent { e.1 += size as u64 } else { e.0 += size as u64 }
            }
        })
        .build()
}

fn file_provider(store: files::Shared) -> Provider {
    Provider::by_guid(KERNEL_FILE)
        .any(FILE_KEYWORDS)
        .add_callback(move |record: &EventRecord, locator: &SchemaLocator| {
            let id = record.event_id();
            // 10 NameCreate, 11 NameDelete, 12 Create, 15 Read, 16 Write
            if !matches!(id, 10 | 11 | 12 | 15 | 16) {
                return;
            }
            let Ok(schema) = locator.event_schema(record) else { return };
            let p = Parser::create(record, &schema);
            let ptr = |name: &str| p.try_parse::<Pointer>(name).map(|v| *v as u64);
            // Parse first, then lock briefly: this callback can run thousands of times a second.
            match id {
                10 => {
                    if let (Ok(key), Ok(name)) = (ptr("FileKey"), p.try_parse::<String>("FileName")) {
                        if let Ok(mut s) = store.lock() { s.name_key(key, name) }
                    }
                }
                11 => {
                    if let Ok(key) = ptr("FileKey") {
                        if let Ok(mut s) = store.lock() { s.forget_key(key) }
                    }
                }
                12 => {
                    if let (Ok(obj), Ok(name)) = (ptr("FileObject"), p.try_parse::<String>("FileName")) {
                        if let Ok(mut s) = store.lock() { s.name_obj(obj, name) }
                    }
                }
                _ => {
                    let (Ok(obj), Ok(size)) = (ptr("FileObject"), p.try_parse::<u32>("IOSize")) else { return };
                    let key = ptr("FileKey").unwrap_or(0);
                    if let Ok(mut s) = store.lock() {
                        s.io(record.process_id(), obj, key, size as u64, id == 16);
                    }
                }
            }
        })
        .build()
}

pub fn start(counters: Counters, files: files::Shared) -> Result<(), String> {
    // A crash can leave our session running; clear it so the name is free.
    for name in std::iter::once(&SESSION).chain(LEGACY_SESSIONS) {
        let _ = std::process::Command::new("logman")
            .args(["stop", name, "-ets"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }

    let trace = UserTrace::new()
        .named(SESSION.to_string())
        .enable(network_provider(counters))
        .enable(file_provider(files))
        .start_and_process()
        .map_err(|e| format!("{e:?}"))?;
    *TRACE.lock().map_err(|e| e.to_string())? = Some(trace);
    Ok(())
}

/// Stops the session; called on app exit so it doesn't outlive us.
pub fn stop() {
    if let Ok(mut t) = TRACE.lock() {
        t.take();
    }
}
