//! Per-process file activity from the Microsoft-Windows-Kernel-File ETW provider.
//!
//! Read/write events only carry kernel handles (FileObject, FileKey), so names are
//! learned from Create (FileObject -> name) and NameCreate (FileKey -> name) events.
//! Files opened before tracing started never get a name.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, QueryDosDeviceW};

const WINDOW_SECS: usize = 10;
const MAX_NAMES: usize = 200_000;

#[derive(Default, Clone, Copy)]
struct Io {
    read: u64,
    write: u64,
    ops: u32,
}

type Bucket = HashMap<(u32, Arc<str>), Io>;

pub struct Store {
    by_key: HashMap<u64, Arc<str>>,
    by_obj: HashMap<u64, Arc<str>>,
    current: Bucket,
    window: VecDeque<Bucket>,
    unknown: Arc<str>,
}

pub type Shared = Arc<Mutex<Store>>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRow {
    pid: u32,
    path: String,
    read: f64,  // bytes/s
    write: f64, // bytes/s
    ops: u32,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            by_key: HashMap::new(),
            by_obj: HashMap::new(),
            current: HashMap::new(),
            window: VecDeque::new(),
            unknown: Arc::from("(opened before Overseer started tracing)"),
        }
    }
}

/// "\Device\HarddiskVolume3" -> "C:" for every mounted drive letter.
fn device_map() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let drive = format!("{}:", (b'A' + i as u8) as char);
        let name: Vec<u16> = drive.encode_utf16().chain(Some(0)).collect();
        let mut buf = [0u16; 512];
        let n = unsafe { QueryDosDeviceW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if n > 0 {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
            out.push((String::from_utf16_lossy(&buf[..end]), drive));
        }
    }
    out
}

fn dos_path(nt: &str, map: &[(String, String)]) -> String {
    for (device, drive) in map {
        if let Some(rest) = nt.strip_prefix(device.as_str()) {
            if rest.is_empty() || rest.starts_with('\\') {
                return format!("{drive}{rest}");
            }
        }
    }
    nt.to_string()
}

impl Store {
    pub fn name_key(&mut self, key: u64, name: String) {
        if self.by_key.len() >= MAX_NAMES {
            self.by_key.clear();
        }
        self.by_key.insert(key, name.into());
    }

    pub fn forget_key(&mut self, key: u64) {
        self.by_key.remove(&key);
    }

    pub fn name_obj(&mut self, obj: u64, name: String) {
        // Close events would flood the trace, so cap this table instead of pruning on close.
        if self.by_obj.len() >= MAX_NAMES {
            self.by_obj.clear();
        }
        self.by_obj.insert(obj, name.into());
    }

    pub fn io(&mut self, pid: u32, obj: u64, key: u64, bytes: u64, write: bool) {
        let name = self
            .by_obj
            .get(&obj)
            .or_else(|| self.by_key.get(&key))
            .cloned()
            .unwrap_or_else(|| self.unknown.clone());
        let e = self.current.entry((pid, name)).or_default();
        if write {
            e.write += bytes;
        } else {
            e.read += bytes;
        }
        e.ops += 1;
    }

    /// Called once a second by the sampler: closes the current one-second bucket.
    pub fn rotate(&mut self) {
        let bucket = std::mem::take(&mut self.current);
        self.window.push_back(bucket);
        while self.window.len() > WINDOW_SECS {
            self.window.pop_front();
        }
    }

    /// Busiest files over the last few seconds, for one process or all of them.
    pub fn top(&self, pid: Option<u32>, n: usize) -> Vec<FileRow> {
        let mut agg: HashMap<(u32, Arc<str>), Io> = HashMap::new();
        for bucket in &self.window {
            for ((p, name), io) in bucket {
                if pid.is_some_and(|want| want != *p) {
                    continue;
                }
                let e = agg.entry((*p, name.clone())).or_default();
                e.read += io.read;
                e.write += io.write;
                e.ops += io.ops;
            }
        }
        let secs = self.window.len().max(1) as f64;
        let map = device_map();
        let mut rows: Vec<FileRow> = agg
            .into_iter()
            .map(|((p, name), io)| FileRow {
                pid: p,
                path: dos_path(&name, &map),
                read: io.read as f64 / secs,
                write: io.write as f64 / secs,
                ops: io.ops,
            })
            .collect();
        rows.sort_by(|a, b| (b.read + b.write).total_cmp(&(a.read + a.write)));
        rows.truncate(n);
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_paths_become_drive_paths() {
        let map = vec![(r"\Device\HarddiskVolume3".to_string(), "C:".to_string())];
        assert_eq!(dos_path(r"\Device\HarddiskVolume3\Users\x.txt", &map), r"C:\Users\x.txt");
        // A longer volume number must not match a shorter prefix.
        assert_eq!(dos_path(r"\Device\HarddiskVolume33\a", &map), r"\Device\HarddiskVolume33\a");
    }

    #[test]
    fn io_is_attributed_to_named_files_and_windowed() {
        let mut s = Store::default();
        s.name_obj(0x10, r"\Device\X\a.log".into());
        s.io(42, 0x10, 0, 4096, true);
        s.io(42, 0x99, 0, 100, false); // never named
        s.rotate();
        let rows = s.top(Some(42), 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, r"\Device\X\a.log");
        assert_eq!(rows[0].write, 4096.0);
        assert!(rows[1].path.contains("before Overseer"));
        assert!(s.top(Some(7), 10).is_empty());
    }
}
