//! Last 30 minutes of samples: system totals every second, plus the processes
//! that mattered at that moment (top by CPU, memory, disk, network, GPU).

use std::collections::{HashSet, VecDeque};

use serde::Serialize;

use crate::{Proc, Snapshot};

const KEEP: usize = 30 * 60;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    t: u64, // unix ms
    cpu: f32,
    mem: f32, // % of total
    gpu: f32,
    disk: f64, // bytes/s read + write
    net: f64,  // bytes/s down + up
    top: String,
    top_cpu: f32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct HistProc {
    pid: u32,
    name: String,
    cpu: f32,
    gpu: f32,
    mem: u64,
    disk: f64,
    net: f64,
}

#[derive(Serialize, Clone)]
pub struct Moment {
    t: u64,
    procs: Vec<HistProc>,
}

#[derive(Default)]
pub struct History {
    points: VecDeque<Point>,
    moments: VecDeque<Moment>,
}

fn top_by<F: Fn(&Proc) -> f64>(procs: &[Proc], n: usize, key: F, out: &mut HashSet<u32>) {
    let mut v: Vec<&Proc> = procs.iter().filter(|p| key(p) > 0.0).collect();
    v.sort_by(|a, b| key(b).total_cmp(&key(a)));
    out.extend(v.into_iter().take(n).map(|p| p.pid));
}

impl History {
    pub fn push(&mut self, t: u64, s: &Snapshot) {
        let top = s.procs.iter().max_by(|a, b| a.cpu.total_cmp(&b.cpu));
        self.points.push_back(Point {
            t,
            cpu: s.cpu,
            mem: s.mem_used as f32 / s.mem_total.max(1) as f32 * 100.0,
            gpu: s.gpu,
            disk: s.disk_read + s.disk_write,
            net: s.net_rx + s.net_tx,
            top: top.map(|p| p.name.clone()).unwrap_or_default(),
            top_cpu: top.map(|p| p.cpu).unwrap_or(0.0),
        });

        let mut keep = HashSet::new();
        top_by(&s.procs, 10, |p| p.cpu as f64, &mut keep);
        top_by(&s.procs, 8, |p| p.mem as f64, &mut keep);
        top_by(&s.procs, 5, |p| p.gpu as f64, &mut keep);
        top_by(&s.procs, 5, |p| p.disk_read + p.disk_write, &mut keep);
        top_by(&s.procs, 5, |p| p.net_rx + p.net_tx, &mut keep);
        self.moments.push_back(Moment {
            t,
            procs: s
                .procs
                .iter()
                .filter(|p| keep.contains(&p.pid))
                .map(|p| HistProc {
                    pid: p.pid,
                    name: p.name.clone(),
                    cpu: p.cpu,
                    gpu: p.gpu,
                    mem: p.mem,
                    disk: p.disk_read + p.disk_write,
                    net: p.net_rx + p.net_tx,
                })
                .collect(),
        });

        while self.points.len() > KEEP {
            self.points.pop_front();
        }
        while self.moments.len() > KEEP {
            self.moments.pop_front();
        }
    }

    pub fn since(&self, t: u64) -> Vec<Point> {
        self.points.iter().filter(|p| p.t > t).cloned().collect()
    }

    /// The stored moment closest to `t`.
    pub fn moment(&self, t: u64) -> Option<Moment> {
        self.moments.iter().min_by_key(|m| m.t.abs_diff(t)).cloned()
    }
}
