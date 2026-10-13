//! GPU usage from the same PDH counters Task Manager uses (no admin needed).
//!
//! Instances look like `pid_1234_luid_0x0_0x14954_phys_0_eng_0_engtype_3d`.
//! Per process: its busiest engine. Overall: the busiest engine summed over all processes.

use std::collections::HashMap;
use std::ffi::c_void;

use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};

const PDH_MORE_DATA: u32 = 0x8000_07D2;
const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;

/// Mirrors PDH_FMT_COUNTERVALUE_ITEM_W with a double value.
#[repr(C)]
struct Item {
    name: *const u16,
    status: u32,
    value: f64,
}

#[derive(Default)]
pub struct Sample {
    pub per_pid: HashMap<u32, (f32, u64)>, // (utilisation %, dedicated bytes)
    pub total: f32,
    pub dedicated: u64,
}

pub struct Gpu {
    query: *mut c_void,
    engine: *mut c_void,
    proc_mem: *mut c_void,
    adapter_mem: *mut c_void,
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

/// "pid_1234_rest" -> (1234, "rest")
fn split_pid(instance: &str) -> Option<(u32, &str)> {
    let rest = instance.strip_prefix("pid_")?;
    let (pid, tail) = rest.split_once('_')?;
    Some((pid.parse().ok()?, tail))
}

impl Gpu {
    pub fn new() -> Option<Self> {
        unsafe {
            let mut query = std::mem::zeroed();
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return None;
            }
            let add = |path: &str| {
                let mut c = std::mem::zeroed();
                let p = wide(path);
                (PdhAddEnglishCounterW(query, p.as_ptr(), 0, &mut c) == 0).then_some(c)
            };
            let (Some(engine), Some(proc_mem), Some(adapter_mem)) = (
                add(r"\GPU Engine(*)\Utilization Percentage"),
                add(r"\GPU Process Memory(*)\Dedicated Usage"),
                add(r"\GPU Adapter Memory(*)\Dedicated Usage"),
            ) else {
                PdhCloseQuery(query);
                return None;
            };
            PdhCollectQueryData(query); // rate counters need a baseline
            Some(Self { query: query as _, engine: engine as _, proc_mem: proc_mem as _, adapter_mem: adapter_mem as _ })
        }
    }

    unsafe fn read(&self, counter: *mut c_void) -> Vec<(String, f64)> {
        let (mut size, mut count) = (0u32, 0u32);
        let rc = PdhGetFormattedCounterArrayW(counter as _, PDH_FMT_DOUBLE | PDH_FMT_NOCAP100, &mut size, &mut count, std::ptr::null_mut());
        if rc != PDH_MORE_DATA {
            return Vec::new();
        }
        let mut buf = vec![0u64; size as usize / 8 + 1];
        let rc = PdhGetFormattedCounterArrayW(counter as _, PDH_FMT_DOUBLE | PDH_FMT_NOCAP100, &mut size, &mut count, buf.as_mut_ptr() as *mut _);
        if rc != 0 {
            return Vec::new();
        }
        let items = buf.as_ptr() as *const Item;
        (0..count as usize)
            .map(|i| &*items.add(i))
            .filter(|it| it.status <= 1) // PDH_CSTATUS_VALID_DATA / NEW_DATA
            .map(|it| (wstr(it.name), it.value))
            .collect()
    }

    pub fn sample(&mut self) -> Sample {
        let mut out = Sample::default();
        unsafe {
            if PdhCollectQueryData(self.query as _) != 0 {
                return out;
            }
            let mut per_engine: HashMap<String, f64> = HashMap::new();
            for (name, v) in self.read(self.engine) {
                let Some((pid, engine)) = split_pid(&name) else { continue };
                let e = out.per_pid.entry(pid).or_default();
                e.0 = e.0.max(v as f32);
                *per_engine.entry(engine.to_string()).or_default() += v;
            }
            out.total = per_engine.values().fold(0.0f64, |a, &b| a.max(b)).min(100.0) as f32;

            for (name, v) in self.read(self.proc_mem) {
                if let Some((pid, _)) = split_pid(&name) {
                    out.per_pid.entry(pid).or_default().1 += v as u64;
                }
            }
            out.dedicated = self.read(self.adapter_mem).iter().map(|(_, v)| *v as u64).sum();
        }
        out
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query as _);
        }
    }
}
