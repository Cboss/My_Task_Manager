//! Code-signature checks and file hashes.
//!
//! Signatures come from PowerShell's Get-AuthenticodeSignature, run in batches on a
//! background thread and cached per path. It handles catalog-signed Windows files
//! (most of System32), which a plain embedded-signature check would wrongly call unsigned.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const BATCH: usize = 80;

#[derive(Serialize, Clone)]
pub struct Sig {
    pub status: &'static str, // valid | unsigned | invalid | unknown
    pub signer: Option<String>,
}

pub type Cache = Arc<Mutex<HashMap<String, Sig>>>;
pub type Wanted = Arc<Mutex<HashSet<String>>>;

#[derive(Deserialize)]
struct Row {
    p: String,
    s: String,
    n: Option<String>,
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn verify_batch(paths: &[String]) -> Vec<(String, Sig)> {
    let list = std::env::temp_dir().join(format!("overseer-sig-{}.txt", std::process::id()));
    if std::fs::write(&list, paths.join("\n")).is_err() {
        return Vec::new();
    }
    let script = format!(
        r#"$ErrorActionPreference = 'SilentlyContinue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$rows = foreach ($f in (Get-Content -LiteralPath '{}' -Encoding UTF8)) {{
  $s = Get-AuthenticodeSignature -LiteralPath $f
  $n = if ($s.SignerCertificate) {{ $s.SignerCertificate.GetNameInfo('SimpleName', $false) }} else {{ $null }}
  [pscustomobject]@{{ p = [string]$f; s = "$($s.Status)"; n = $n }}
}}
ConvertTo-Json -InputObject @($rows) -Compress"#,
        list.display().to_string().replace('\'', "''")
    );
    let encoded: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &base64(&encoded)])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = std::fs::remove_file(&list);
    let Ok(out) = out else { return Vec::new() };
    let rows: Vec<Row> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    rows.into_iter()
        .map(|r| {
            let status = match r.s.as_str() {
                "Valid" => "valid",
                "NotSigned" => "unsigned",
                "HashMismatch" | "NotTrusted" => "invalid",
                _ => "unknown",
            };
            (r.p, Sig { status, signer: r.n.filter(|n| !n.is_empty()) })
        })
        .collect()
}

/// Background loop: verify whatever paths the sampler has asked about.
pub fn spawn(cache: Cache, wanted: Wanted) {
    std::thread::spawn(move || loop {
        let batch: Vec<String> = {
            let mut w = wanted.lock().unwrap();
            let take: Vec<String> = w.iter().take(BATCH).cloned().collect();
            for p in &take {
                w.remove(p);
            }
            take
        };
        if batch.is_empty() {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        // Placeholder first: the sampler stops asking, and failures aren't retried forever.
        {
            let mut c = cache.lock().unwrap();
            for p in &batch {
                c.insert(p.clone(), Sig { status: "unknown", signer: None });
            }
        }
        let results = verify_batch(&batch);
        let mut c = cache.lock().unwrap();
        for (p, s) in results {
            c.insert(p, s);
        }
    });
}

pub fn sha256(path: &str) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("Can't read the file: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
