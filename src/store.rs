//! The Responses API state: an in-memory cache plus, when a directory is configured, one small JSON file per response
//! holding only what that response added (parent id, new input turns, output); system prompts and tool lists are
//! stored once per distinct value (by content hash). The format is the same since 0.0.1, so chains survive upgrades.
//! Files are owner-only (0600, folders 0700); purged at startup and hourly.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canonical::{hex, CanonicalRequest, CanonicalResponse, Finish, ToolCall, ToolSpec, Turn, Usage};
use crate::emulation::followups::sorted_json;

const MEMORY_MAX: usize = 500;
/// Longest chain rebuilt from disk (a guard against a corrupt parent loop).
const MAX_CHAIN: usize = 10_000;

/// (created at, request, response)
pub type Entry = (f64, Arc<CanonicalRequest>, Arc<CanonicalResponse>);

pub fn time_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// One response on disk.
#[derive(Serialize, Deserialize)]
struct Record {
    #[serde(default = "one")]
    v: u32,
    id: String,
    ts: f64,
    parent: Option<String>,
    /// blob hash of the system prompts (a list of strings)
    system: Option<String>,
    /// blob hash of the tool list
    tools: Option<String>,
    /// the turns this response added to its parent's conversation
    turns: Vec<Turn>,
    resp: StoredResponse,
}

fn one() -> u32 {
    1
}

#[derive(Serialize, Deserialize)]
struct StoredResponse {
    text: String,
    tool_calls: Vec<ToolCall>,
    finish: String,
    usage: Usage,
}

pub struct ResponseStore {
    pub dir: Option<PathBuf>,
    retention_s: f64,
    max_bytes: u64,
    memory: Mutex<IndexMap<String, Entry>>,
}

/// A number without a pointless ".0" (30, 0.5).
fn short(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Refresh a file's modification time (blobs live as long as something still uses them).
fn touch(path: &Path) -> std::io::Result<()> {
    fs::File::options().write(true).open(path)?.set_modified(SystemTime::now())
}

/// Our ids only: anything else never reaches the file system.
fn valid_id(rid: &str) -> bool {
    rid.len() == 29 && rid.starts_with("resp_") && rid[5..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let raw = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_json<T: Serialize>(path: &Path, data: &T) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec(data)?)?;
    }
    fs::rename(&tmp, path)
}

impl ResponseStore {
    pub fn new(directory: Option<PathBuf>, retention_days: f64, max_mb: f64) -> Self {
        let mut store = ResponseStore {
            dir: None,
            retention_s: retention_days * 86400.0,
            max_bytes: (max_mb * 1024.0 * 1024.0) as u64,
            memory: Mutex::new(IndexMap::new()),
        };
        let Some(dir) = directory else { return store };
        let setup = || -> std::io::Result<()> {
            fs::create_dir_all(dir.join("blobs"))?;
            for d in [dir.clone(), dir.join("blobs")] {
                fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        };
        if let Err(e) = setup() {
            tracing::warn!(
                "responses store: cannot use {} ({e}); previous_response_id is kept in memory only (lost on restart)",
                dir.display()
            );
            return store;
        }
        tracing::info!(
            "responses store: {} (retention {} days from creation, max {} MB)",
            dir.display(),
            short(retention_days),
            short(max_mb)
        );
        store.dir = Some(dir);
        store.purge();
        store
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, IndexMap<String, Entry>> {
        self.memory.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn describe(&self) -> String {
        match &self.dir {
            Some(d) => format!("kept {} days in {}", short(self.retention_s / 86400.0), d.display()),
            None => "kept in memory only; a restart forgets them".into(),
        }
    }

    /// The telemetry session of a stored response (memory only).
    pub fn session_of(&self, rid: &str) -> Option<String> {
        self.mem().get(rid).map(|e| e.1.meta().session.clone())
    }

    /// Store a value once, by the hash of its content; None for an empty value.
    fn blob<T: Serialize>(&self, dir: &Path, data: &[T]) -> std::io::Result<Option<String>> {
        if data.is_empty() {
            return Ok(None);
        }
        let value = serde_json::to_value(data)?;
        let h = hex(&Sha256::digest(sorted_json(&value).as_bytes()))[..32].to_string();
        let f = dir.join("blobs").join(format!("{h}.json"));
        if f.exists() {
            touch(&f)?;
        } else {
            write_json(&f, &value)?;
        }
        Ok(Some(h))
    }

    fn unblob<T: serde::de::DeserializeOwned>(&self, dir: &Path, hash: &str) -> Result<T, String> {
        if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid blob name {hash:?}"));
        }
        let f = dir.join("blobs").join(format!("{hash}.json"));
        touch(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        read_json(&f)
    }

    pub fn load(&self, rid: &str) -> Option<Entry> {
        {
            let mut mem = self.mem();
            if let Some(hit) = mem.get(rid) {
                if time_now() - hit.0 <= self.retention_s {
                    return Some(hit.clone());
                }
                mem.shift_remove(rid);
            }
        }
        let dir = self.dir.as_ref().filter(|_| valid_id(rid))?;
        match self.rebuild(dir, rid) {
            Ok(Some(entry)) => {
                self.mem().insert(rid.to_string(), entry.clone());
                Some(entry)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("responses store: could not rebuild {rid}: {e}");
                None
            }
        }
    }

    /// The conversation up to `rid`, from its chain of records (oldest first).
    fn rebuild(&self, dir: &Path, rid: &str) -> Result<Option<Entry>, String> {
        let mut chain: Vec<Record> = vec![];
        let mut cur = Some(rid.to_string());
        while let Some(id) = cur.filter(|c| !c.is_empty()) {
            let f = dir.join(format!("{id}.json"));
            if !f.exists() {
                if !chain.is_empty() {
                    tracing::warn!("responses store: {id} is missing from the chain of {rid} (purged?)");
                }
                return Ok(None);
            }
            let rec: Record = read_json(&f)?;
            cur = rec.parent.clone();
            chain.push(rec);
            if chain.len() > MAX_CHAIN {
                return Err("chain too long".into());
            }
        }
        chain.reverse();
        let mut req = CanonicalRequest::default();
        let last = chain.len() - 1;
        for (k, rec) in chain.iter().enumerate() {
            if let Some(h) = &rec.system {
                req.system = self.unblob(dir, h)?;
            }
            if let Some(h) = &rec.tools {
                req.tools = self.unblob::<Vec<ToolSpec>>(dir, h)?;
            }
            req.turns.extend(rec.turns.iter().cloned());
            if k < last {
                let mut t = Turn::new("assistant", &rec.resp.text);
                t.tool_calls = rec.resp.tool_calls.clone();
                req.turns.push(t);
            }
        }
        let rec = &chain[last];
        let resp = CanonicalResponse {
            text: rec.resp.text.clone(),
            tool_calls: rec.resp.tool_calls.clone(),
            finish: Finish::parse(&rec.resp.finish),
            usage: rec.resp.usage,
            ..Default::default()
        };
        Ok(Some((rec.ts, Arc::new(req), Arc::new(resp))))
    }

    pub fn remember(&self, rid: &str, req: Arc<CanonicalRequest>, resp: Arc<CanonicalResponse>) {
        let now = time_now();
        {
            let mut mem = self.mem();
            mem.insert(rid.to_string(), (now, req.clone(), resp.clone()));
            while mem.len() > MEMORY_MAX {
                mem.shift_remove_index(0);
            }
        }
        let Some(dir) = &self.dir else { return };
        let write = || -> std::io::Result<()> {
            let (parent, prev_turns) = {
                let m = req.meta();
                (m.prev_id.clone(), m.prev_turns)
            };
            let record = Record {
                v: 1,
                id: rid.to_string(),
                ts: now,
                parent,
                system: self.blob(dir, &req.system)?,
                tools: self.blob(dir, &req.tools)?,
                turns: req.turns.iter().skip(prev_turns).cloned().collect(),
                resp: StoredResponse {
                    text: resp.text.clone(),
                    tool_calls: resp.tool_calls.clone(),
                    finish: resp.finish.to_string(),
                    usage: resp.usage,
                },
            };
            write_json(&dir.join(format!("{rid}.json")), &record)
        };
        if let Err(e) = write() {
            tracing::warn!("responses store: could not persist {rid}: {e}");
        }
    }

    /// Retention is fixed from creation; then, above responses_max_mb, the oldest responses go until the store is
    /// back under 90% of the cap. Runs at startup and hourly.
    pub fn purge(&self) -> usize {
        let Some(dir) = &self.dir else { return 0 };
        let cutoff = time_now() - self.retention_s;
        let list = |d: &Path, prefix: &str| -> Vec<(PathBuf, f64, u64)> {
            fs::read_dir(d)
                .map(|it| {
                    it.filter_map(Result::ok)
                        .map(|e| e.path())
                        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix) && n.ends_with(".json")))
                        .filter_map(|p| {
                            let m = fs::metadata(&p).ok()?;
                            let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
                            Some((p, mtime, m.len()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut removed = 0;
        let mut responses = vec![];
        let mut total: u64 = 0;
        for (f, mtime, size) in list(dir, "resp_").into_iter().chain(list(&dir.join("blobs"), "")) {
            if mtime < cutoff {
                let _ = fs::remove_file(&f);
                removed += 1;
                continue;
            }
            total += size;
            if f.parent() == Some(dir.as_path()) {
                responses.push((mtime, size, f));
            }
        }
        if total > self.max_bytes {
            responses.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, size, f) in responses {
                if (total as f64) <= self.max_bytes as f64 * 0.9 {
                    break;
                }
                let _ = fs::remove_file(&f);
                if let Some(stem) = f.file_stem().and_then(|s| s.to_str()) {
                    self.mem().shift_remove(stem);
                }
                total -= size;
                removed += 1;
            }
            tracing::warn!("responses store: above {} MB, removed the oldest responses", self.max_bytes / (1024 * 1024));
        }
        if removed > 0 {
            tracing::info!("responses store: purged {removed} files");
        }
        removed
    }
}
