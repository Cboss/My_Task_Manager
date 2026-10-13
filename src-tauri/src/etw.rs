//! Per-process network bandwidth from the Microsoft-Windows-Kernel-Network ETW
//! provider. Starting a trace session needs administrator rights.

use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::sync::{Arc, Mutex};

use ferrisetw::parser::Parser;
use ferrisetw::provider::Provider;
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::UserTrace;
use ferrisetw::EventRecord;

const SESSION: &str = "Overseer-Net";
const KERNEL_NETWORK: &str = "7dd42a49-5329-4832-8dfd-43d979153a88";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Bytes (received, sent) per PID since the last drain.
pub type Counters = Arc<Mutex<HashMap<u32, (u64, u64)>>>;

static TRACE: Mutex<Option<UserTrace>> = Mutex::new(None);

pub fn start(counters: Counters) -> Result<(), String> {
    // A crash can leave our session running; clear it so the name is free.
    let _ = std::process::Command::new("logman")
        .args(["stop", SESSION, "-ets"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();

    let provider = Provider::by_guid(KERNEL_NETWORK)
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
        .build();

    let trace = UserTrace::new()
        .named(SESSION.to_string())
        .enable(provider)
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
