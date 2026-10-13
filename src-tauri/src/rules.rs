//! Alert rules, evaluated in the background sampler so they keep working while
//! the window is hidden. Rules are saved to rules.json in the app config dir.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::Proc;

const COOLDOWN: Duration = Duration::from_secs(60);
const LOG_LIMIT: usize = 100;

#[derive(Serialize, Deserialize, Clone)]
pub struct Rule {
    pub id: String,
    pub r#match: String,
    pub metric: String, // "cpu" (%) | "mem" (MB)
    pub threshold: f64,
    pub duration: u64, // seconds
    pub action: String, // "notify" | "lower" | "kill"
    pub enabled: bool,
}

#[derive(Serialize, Clone)]
pub struct LogEntry {
    time: u64, // unix ms
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RulesView {
    rules: Vec<Rule>,
    log: Vec<LogEntry>,
    last_fired: HashMap<String, u64>,
}

type Key = (String, u32); // (rule id, pid)

pub struct Engine {
    rules: Vec<Rule>,
    path: Option<PathBuf>,
    since: HashMap<Key, Instant>,
    fired: HashMap<Key, Instant>,
    last_fired: HashMap<String, u64>,
    log: VecDeque<LogEntry>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Engine {
    pub fn load(path: Option<PathBuf>) -> Self {
        let rules = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            rules,
            path,
            since: HashMap::new(),
            fired: HashMap::new(),
            last_fired: HashMap::new(),
            log: VecDeque::new(),
        }
    }

    pub fn view(&self) -> RulesView {
        RulesView {
            rules: self.rules.clone(),
            log: self.log.iter().cloned().collect(),
            last_fired: self.last_fired.clone(),
        }
    }

    pub fn set(&mut self, rules: Vec<Rule>) -> Result<(), String> {
        self.rules = rules;
        self.since.clear();
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(&self.rules).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }

    pub fn clear_log(&mut self) {
        self.log.clear();
    }

    fn log(&mut self, text: String) {
        self.log.push_front(LogEntry { time: now_ms(), text });
        self.log.truncate(LOG_LIMIT);
    }

    /// Check every enabled rule against this sample and act on the ones that have
    /// been over their limit long enough.
    pub fn run(&mut self, procs: &[Proc], app: &AppHandle, kill: &dyn Fn(u32) -> bool) {
        let now = Instant::now();
        let mut seen = HashSet::new();
        let mut firings = Vec::new();

        for r in self.rules.iter().filter(|r| r.enabled) {
            let needle = r.r#match.to_lowercase();
            let mut hits = Vec::new();
            for p in procs {
                if !p.name.to_lowercase().contains(&needle) {
                    continue;
                }
                let value = if r.metric == "cpu" { p.cpu as f64 } else { p.mem as f64 / 1_048_576.0 };
                if value <= r.threshold {
                    continue;
                }
                let key = (r.id.clone(), p.pid);
                seen.insert(key.clone());
                let since = *self.since.entry(key.clone()).or_insert(now);
                if now.duration_since(since) < Duration::from_secs(r.duration) {
                    continue;
                }
                if self.fired.get(&key).is_some_and(|t| now.duration_since(*t) < COOLDOWN) {
                    continue;
                }
                self.fired.insert(key, now);
                hits.push((p.pid, p.name.clone(), value));
            }
            if !hits.is_empty() {
                firings.push((r.clone(), hits));
            }
        }

        self.since.retain(|k, _| seen.contains(k));
        self.fired.retain(|_, t| now.duration_since(*t) < COOLDOWN);
        for (rule, hits) in firings {
            self.fire(&rule, hits, app, kill);
        }
    }

    /// Acts on each matching process but sends one notification per rule.
    fn fire(&mut self, r: &Rule, hits: Vec<(u32, String, f64)>, app: &AppHandle, kill: &dyn Fn(u32) -> bool) {
        let cpu = r.metric == "cpu";
        let fmt = |v: f64| if cpu { format!("{v:.0}% CPU") } else { format!("{v:.0} MB") };
        let limit = if cpu { format!("{}% CPU", r.threshold) } else { format!("{} MB memory", r.threshold) };
        let verb = match r.action.as_str() { "kill" => "Ended", "lower" => "Lowered priority of", _ => "Alert:" };

        let mut done = 0;
        for (pid, name, value) in &hits {
            let ok = match r.action.as_str() {
                "kill" => kill(*pid),
                "lower" => crate::win::set_priority(*pid, "below").is_ok(),
                _ => true,
            };
            if ok {
                done += 1;
                self.log(format!("{verb} {name} ({pid}) at {} for {}s+", fmt(*value), r.duration));
            } else {
                self.log(format!("Rule failed on {name} ({pid}): access denied or already exited"));
            }
        }
        self.last_fired.insert(r.id.clone(), now_ms());
        if done == 0 {
            return;
        }

        let mut names: Vec<&str> = hits.iter().map(|h| h.1.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let names = names.join(", ");
        let peak = fmt(hits.iter().map(|h| h.2).fold(0.0, f64::max));
        let count = if hits.len() > 1 { format!("{} processes", hits.len()) } else { format!("PID {}", hits[0].0) };
        let title = match r.action.as_str() {
            "kill" => format!("Overseer ended {names}"),
            "lower" => format!("Overseer lowered priority of {names}"),
            _ => format!("{names} is over {limit}"),
        };
        let _ = app
            .notification()
            .builder()
            .title(title)
            .body(format!("{count}, peak {peak}, for {}s or more.", r.duration))
            .show();
    }
}
