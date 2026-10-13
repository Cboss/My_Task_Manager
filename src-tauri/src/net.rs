//! Per-process TCP/UDP connections via the IP Helper API (no admin needed).

use std::collections::HashMap;
use std::ffi::c_void;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde::Serialize;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

#[repr(C)]
struct Tcp4Row { state: u32, local_addr: u32, local_port: u32, remote_addr: u32, remote_port: u32, pid: u32 }
#[repr(C)]
struct Tcp6Row { local_addr: [u8; 16], local_scope: u32, local_port: u32, remote_addr: [u8; 16], remote_scope: u32, remote_port: u32, state: u32, pid: u32 }
#[repr(C)]
struct Udp4Row { local_addr: u32, local_port: u32, pid: u32 }
#[repr(C)]
struct Udp6Row { local_addr: [u8; 16], local_scope: u32, local_port: u32, pid: u32 }

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Conn {
    pub pid: u32,
    pub proto: &'static str,
    pub local: String,
    pub remote: String,
    pub state: &'static str,
}

#[derive(Default, Clone, Copy)]
pub struct Counts {
    pub established: u32,
    pub listening: u32,
}

fn port(p: u32) -> u16 {
    u16::from_be((p & 0xffff) as u16)
}

fn tcp_state(s: u32) -> &'static str {
    match s {
        1 => "Closed", 2 => "Listen", 3 => "SynSent", 4 => "SynRecv", 5 => "Established",
        6 => "FinWait1", 7 => "FinWait2", 8 => "CloseWait", 9 => "Closing", 10 => "LastAck",
        11 => "TimeWait", 12 => "DeleteTcb", _ => "?",
    }
}

/// Calls a GetExtended*Table function, growing the buffer until it fits.
/// Returns the rows as a typed vector.
fn fetch<R>(call: impl Fn(*mut c_void, *mut u32) -> u32) -> Vec<R> {
    let mut size: u32 = 0;
    call(std::ptr::null_mut(), &mut size);
    for _ in 0..4 {
        // u64 buffer keeps the table 8-byte aligned.
        let mut buf = vec![0u64; (size as usize).div_ceil(8) + 1];
        let rc = call(buf.as_mut_ptr() as *mut c_void, &mut size);
        if rc == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if rc != 0 {
            return Vec::new();
        }
        unsafe {
            let base = buf.as_ptr() as *const u8;
            let n = std::ptr::read_unaligned(base as *const u32) as usize;
            // Rows start after the u32 count, padded to the row alignment (4 here).
            let rows = base.add(4) as *const R;
            return (0..n).map(|i| std::ptr::read_unaligned(rows.add(i))).collect();
        }
    }
    Vec::new()
}

fn tcp<R>(af: u32) -> Vec<R> {
    fetch(|p, s| unsafe { GetExtendedTcpTable(p, s, 0, af, TCP_TABLE_OWNER_PID_ALL, 0) })
}

fn udp<R>(af: u32) -> Vec<R> {
    fetch(|p, s| unsafe { GetExtendedUdpTable(p, s, 0, af, UDP_TABLE_OWNER_PID, 0) })
}

/// Connection counts per PID, cheap enough to run every tick.
pub fn counts() -> HashMap<u32, Counts> {
    let mut map: HashMap<u32, Counts> = HashMap::new();
    let mut add = |pid: u32, state: u32| {
        let c = map.entry(pid).or_default();
        match state {
            5 => c.established += 1,
            2 => c.listening += 1,
            _ => {}
        }
    };
    for r in tcp::<Tcp4Row>(AF_INET) { add(r.pid, r.state) }
    for r in tcp::<Tcp6Row>(AF_INET6) { add(r.pid, r.state) }
    map
}

/// Full connection list for one process.
pub fn for_pid(pid: u32) -> Vec<Conn> {
    let v4 = |a: u32, p: u32| format!("{}:{}", Ipv4Addr::from(a.to_ne_bytes()), port(p));
    let v6 = |a: [u8; 16], p: u32| format!("[{}]:{}", Ipv6Addr::from(a), port(p));
    let mut out = Vec::new();
    for r in tcp::<Tcp4Row>(AF_INET).into_iter().filter(|r| r.pid == pid) {
        out.push(Conn { pid, proto: "TCP", local: v4(r.local_addr, r.local_port), remote: v4(r.remote_addr, r.remote_port), state: tcp_state(r.state) });
    }
    for r in tcp::<Tcp6Row>(AF_INET6).into_iter().filter(|r| r.pid == pid) {
        out.push(Conn { pid, proto: "TCP6", local: v6(r.local_addr, r.local_port), remote: v6(r.remote_addr, r.remote_port), state: tcp_state(r.state) });
    }
    for r in udp::<Udp4Row>(AF_INET).into_iter().filter(|r| r.pid == pid) {
        out.push(Conn { pid, proto: "UDP", local: v4(r.local_addr, r.local_port), remote: String::new(), state: "" });
    }
    for r in udp::<Udp6Row>(AF_INET6).into_iter().filter(|r| r.pid == pid) {
        out.push(Conn { pid, proto: "UDP6", local: v6(r.local_addr, r.local_port), remote: String::new(), state: "" });
    }
    // Established first, then listeners, then the rest.
    let rank = |s: &str| match s { "Established" => 0, "Listen" => 1, "" => 2, _ => 3 };
    out.sort_by_key(|c| rank(c.state));
    out
}
