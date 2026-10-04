//! The Responses API state: an in-memory cache plus, when a directory is configured, one small JSON file per response
//! holding only what that response added (parent id, new input turns, output, model and echoed settings); system
//! prompts, tool lists and echoed tool definitions are stored once per distinct value (blobs, by content hash). The
//! format is the same since 0.0.1 (newer fields are optional), so chains survive upgrades. Files are owner-only (0600,
//! folders 0700); purged at startup and hourly. Disk work runs on the blocking pool, never on the async workers.
//!
//! Memory: a response shares its history with the response it continues (turns are reference-counted), so a chain
//! costs what each response adds, not a copy of the conversation per response. The cache is capped by those bytes
//! (`responses_memory_mb`), least recently used out first; a miss is rebuilt from disk.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::canonical::{CanonicalRequest, CanonicalResponse, Finish, ToolCall, ToolSpec, Turn, Usage, hex};
use crate::config::DEFAULT_MODEL_NAME;
use crate::json::sorted;

/// Longest chain rebuilt from disk (a guard against a corrupt parent loop).
const MAX_CHAIN: usize = 10_000;
/// What an entry costs besides its content (the request and response structures, ids, the cache slot).
const ENTRY_OVERHEAD: usize = 1024;
/// Blobs and temporary files younger than this are never collected: a write may be about to reference them.
const GRACE: Duration = Duration::from_secs(600);

/// The request settings a Response object echoes, as the client sent them (raw JSON, one line each).
pub type Echo = IndexMap<String, Box<RawValue>>;

pub fn time_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// One response: its id and creation time, the model it was asked for, the settings it echoes, the request it
/// answered (with the shared history) and the answer.
#[derive(Clone)]
pub struct Stored {
    pub id: String,
    pub created: f64,
    pub model: String,
    pub echo: Arc<Echo>,
    pub req: Arc<CanonicalRequest>,
    pub resp: Arc<CanonicalResponse>,
}

impl Stored {
    pub fn new(id: &str, model: &str, echo: Arc<Echo>, req: Arc<CanonicalRequest>, resp: CanonicalResponse) -> Self {
        Stored { id: id.into(), created: time_now(), model: model.into(), echo, req, resp: Arc::new(resp) }
    }
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
    turns: Vec<Arc<Turn>>,
    resp: StoredResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// the echoed settings, without `tools`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    echo: Option<Echo>,
    /// blob hash of the echoed `tools` (the client's own definitions)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    echo_tools: Option<String>,
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

/// The blobs a record uses (what the size purge needs to know, without decoding the turns into memory).
#[derive(Deserialize)]
struct Refs {
    system: Option<String>,
    tools: Option<String>,
    echo_tools: Option<String>,
}

impl Refs {
    fn hashes(self) -> impl Iterator<Item = String> {
        [self.system, self.tools, self.echo_tools].into_iter().flatten()
    }
}

struct Entry {
    stored: Stored,
    /// what this entry is charged for: its own turns, its system prompts and tools when not shared, its answer (and
    /// the history of evicted ancestors it keeps alive)
    bytes: usize,
    /// the part freed with the entry itself (its answer); the rest is history a continuation may share
    answer: usize,
    parent: Option<String>,
}

#[derive(Default)]
struct Cache {
    /// least recently used first
    entries: IndexMap<String, Entry>,
    bytes: usize,
}

pub struct ResponseStore {
    pub dir: Option<PathBuf>,
    retention_s: f64,
    max_bytes: u64,
    memory_max: usize,
    memory: Mutex<Cache>,
    /// writes take it shared, the purge exclusively: a blob is never removed while a record that uses it is written
    disk: RwLock<()>,
}

/// A number without a pointless ".0" (30, 0.5).
fn short(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") }
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

/// Write a file whole or not at all: a temporary file of its own (two writers of the same blob never share one), then
/// a rename over the target.
fn write_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let tmp = path.with_file_name(format!("{name}.{}.tmp", crate::canonical::hex_id(12)));
    {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(data)?;
    }
    fs::rename(&tmp, path)
}

/// What a stored response's answer costs in memory.
fn answer_bytes(s: &Stored) -> usize {
    s.resp.text.len()
        + s.resp.tool_calls.iter().map(|c| c.id.len() + c.name.len() + crate::canonical::value_bytes(&c.arguments)).sum::<usize>()
}

/// What a stored response costs in memory beyond what it shares with the response it continues.
fn entry_bytes(s: &Stored, parent: Option<&Stored>) -> usize {
    let shared = |i: usize, t: &Arc<Turn>| parent.and_then(|p| p.req.turns.get(i)).is_some_and(|pt| Arc::ptr_eq(pt, t));
    let turns: usize = s.req.turns.iter().enumerate().filter(|(i, t)| !shared(*i, t)).map(|(_, t)| t.approx_bytes()).sum();
    let system =
        if parent.is_some_and(|p| Arc::ptr_eq(&p.req.system, &s.req.system)) { 0 } else { s.req.system.iter().map(String::len).sum() };
    let tools = if parent.is_some_and(|p| Arc::ptr_eq(&p.req.tools, &s.req.tools)) {
        0
    } else {
        s.req.tools.iter().map(ToolSpec::approx_bytes).sum()
    };
    let echo: usize = s.echo.iter().map(|(k, v)| k.len() + v.get().len()).sum();
    ENTRY_OVERHEAD + turns + system + tools + echo + answer_bytes(s)
}

impl ResponseStore {
    pub fn new(directory: Option<PathBuf>, retention_days: f64, max_mb: f64, memory_mb: f64) -> Self {
        let mut store = ResponseStore {
            dir: None,
            retention_s: retention_days * 86400.0,
            max_bytes: (max_mb * 1024.0 * 1024.0) as u64,
            memory_max: (memory_mb * 1024.0 * 1024.0) as usize,
            memory: Mutex::new(Cache::default()),
            disk: RwLock::new(()),
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
            "responses store: {} (retention {} days from creation, max {} MB on disk, {} MB in memory)",
            dir.display(),
            short(retention_days),
            short(max_mb),
            short(memory_mb)
        );
        store.dir = Some(dir);
        store.purge();
        store
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.memory.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn describe(&self) -> String {
        match &self.dir {
            Some(d) => format!("kept {} days in {}", short(self.retention_s / 86400.0), d.display()),
            None => "kept in memory only; a restart forgets them".into(),
        }
    }

    /// For /health: the memory cache's size.
    pub fn stats(&self) -> Value {
        let m = self.mem();
        json!({"entries": m.entries.len(), "bytes": m.bytes, "max_bytes": self.memory_max})
    }

    /// The telemetry session of a stored response (memory only).
    pub fn session_of(&self, rid: &str) -> Option<String> {
        self.mem().entries.get(rid).map(|e| e.stored.req.meta().session.clone())
    }

    /// Insert into the memory cache, then drop the least recently used entries above the budget (never the new one).
    /// An evicted response's history stays alive in the cached response that continues it, so that one is charged for
    /// it: the budget counts what memory holds.
    fn cache(&self, stored: Stored) {
        let mut m = self.mem();
        let parent = stored.req.meta().prev_id.clone();
        let bytes = entry_bytes(&stored, parent.as_deref().and_then(|p| m.entries.get(p)).map(|e| &e.stored));
        let answer = answer_bytes(&stored);
        if let Some(old) = m.entries.shift_remove(&stored.id) {
            m.bytes -= old.bytes;
        }
        m.bytes += bytes;
        m.entries.insert(stored.id.clone(), Entry { stored, bytes, answer, parent });
        while m.bytes > self.memory_max && m.entries.len() > 1 {
            let Some((id, gone)) = m.entries.shift_remove_index(0) else { break };
            let history = gone.bytes - gone.answer;
            match m.entries.values_mut().find(|e| e.parent.as_deref() == Some(id.as_str())) {
                Some(child) => {
                    child.bytes += history;
                    m.bytes -= gone.answer;
                }
                None => m.bytes -= gone.bytes,
            }
        }
    }

    /// Keep a response: in memory at once, on disk before this returns (the client may continue the chain right away).
    pub async fn remember(self: &Arc<Self>, stored: Stored) {
        self.cache(stored.clone());
        let Some(dir) = self.dir.clone() else { return };
        let this = self.clone();
        let id = stored.id.clone();
        let written = tokio::task::spawn_blocking(move || this.persist(&dir, &stored)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("responses store: could not persist {id}: {e}"),
            Err(e) => tracing::warn!("responses store: could not persist {id}: {e}"),
        }
    }

    /// A stored response: from memory, else rebuilt from its chain on disk (then cached).
    pub async fn load(self: &Arc<Self>, rid: &str) -> Option<Stored> {
        {
            let mut m = self.mem();
            if let Some(i) = m.entries.get_index_of(rid) {
                if time_now() - m.entries[i].stored.created <= self.retention_s {
                    let last = m.entries.len() - 1;
                    m.entries.move_index(i, last);
                    return Some(m.entries[last].stored.clone());
                }
                if let Some((_, old)) = m.entries.shift_remove_index(i) {
                    m.bytes -= old.bytes;
                }
            }
        }
        let dir = self.dir.clone().filter(|_| valid_id(rid))?;
        let this = self.clone();
        let id = rid.to_string();
        match tokio::task::spawn_blocking(move || this.rebuild(&dir, &id)).await {
            Ok(Ok(Some(stored))) => {
                self.cache(stored.clone());
                Some(stored)
            }
            Ok(Ok(None)) => None,
            Ok(Err(e)) => {
                tracing::warn!("responses store: could not rebuild {rid}: {e}");
                None
            }
            Err(e) => {
                tracing::warn!("responses store: could not rebuild {rid}: {e}");
                None
            }
        }
    }

    fn blob_path(dir: &Path, hash: &str) -> PathBuf {
        dir.join("blobs").join(format!("{hash}.json"))
    }

    /// Store bytes once, under the hash of `key`; returns the hash.
    fn put_blob(dir: &Path, key: &[u8], data: impl FnOnce() -> std::io::Result<Vec<u8>>) -> std::io::Result<String> {
        let h = hex(&Sha256::digest(key))[..32].to_string();
        let f = Self::blob_path(dir, &h);
        match touch(&f) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_file(&f, &data()?)?,
            Err(e) => return Err(e),
        }
        Ok(h)
    }

    /// A list stored once, by the hash of its content; None for an empty list.
    fn blob<T: Serialize>(dir: &Path, data: &[T]) -> std::io::Result<Option<String>> {
        if data.is_empty() {
            return Ok(None);
        }
        let value = serde_json::to_value(data)?;
        let key = sorted(&value);
        Self::put_blob(dir, key.as_bytes(), || Ok(serde_json::to_vec(&value)?)).map(Some)
    }

    fn unblob<T: serde::de::DeserializeOwned>(dir: &Path, hash: &str) -> Result<T, String> {
        if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid blob name {hash:?}"));
        }
        let f = Self::blob_path(dir, hash);
        touch(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        read_json(&f)
    }

    fn persist(&self, dir: &Path, s: &Stored) -> std::io::Result<()> {
        let _writing = self.disk.read().unwrap_or_else(|e| e.into_inner());
        let (parent, prev_turns) = {
            let m = s.req.meta();
            (m.prev_id.clone(), m.prev_turns)
        };
        let mut echo = (*s.echo).clone();
        let echo_tools = match echo.shift_remove("tools") {
            Some(raw) => Some(Self::put_blob(dir, raw.get().as_bytes(), || Ok(raw.get().as_bytes().to_vec()))?),
            None => None,
        };
        let record = Record {
            v: 1,
            id: s.id.clone(),
            ts: s.created,
            parent,
            system: Self::blob(dir, &s.req.system)?,
            tools: Self::blob(dir, &s.req.tools)?,
            turns: s.req.turns.get(prev_turns..).unwrap_or_default().to_vec(),
            resp: StoredResponse {
                text: s.resp.text.clone(),
                tool_calls: s.resp.tool_calls.clone(),
                finish: s.resp.finish.to_string(),
                usage: s.resp.usage,
            },
            model: Some(s.model.clone()),
            echo: (!echo.is_empty()).then_some(echo),
            echo_tools,
        };
        write_file(&dir.join(format!("{}.json", s.id)), &serde_json::to_vec(&record)?)
    }

    /// The conversation up to `rid`, from its chain of records (oldest first).
    fn rebuild(&self, dir: &Path, rid: &str) -> Result<Option<Stored>, String> {
        let _reading = self.disk.read().unwrap_or_else(|e| e.into_inner());
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
        let (mut system_hash, mut tools_hash) = (None, None);
        let last = chain.len() - 1;
        for (k, rec) in chain.iter_mut().enumerate() {
            // consecutive records usually name the same blobs: read each once
            if rec.system.is_some() && rec.system != system_hash {
                req.system = Self::unblob::<Vec<String>>(dir, rec.system.as_deref().unwrap_or_default())?.into();
                system_hash = rec.system.clone();
            }
            if rec.tools.is_some() && rec.tools != tools_hash {
                req.tools = Self::unblob::<Vec<ToolSpec>>(dir, rec.tools.as_deref().unwrap_or_default())?.into();
                tools_hash = rec.tools.clone();
            }
            req.turns.append(&mut rec.turns);
            if k < last {
                let mut t = Turn::new("assistant", &rec.resp.text);
                t.tool_calls = rec.resp.tool_calls.clone();
                req.turns.push(Arc::new(t));
            }
        }
        let rec = chain.pop().ok_or("empty chain")?;
        let mut echo = rec.echo.unwrap_or_default();
        if let Some(h) = &rec.echo_tools {
            let f = Self::blob_path(dir, h);
            let text = fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
            echo.insert("tools".into(), RawValue::from_string(text).map_err(|e| format!("{}: {e}", f.display()))?);
        }
        let resp = CanonicalResponse {
            text: rec.resp.text,
            tool_calls: rec.resp.tool_calls,
            finish: Finish::parse(&rec.resp.finish),
            usage: rec.resp.usage,
            ..Default::default()
        };
        Ok(Some(Stored {
            id: rec.id,
            created: rec.ts,
            model: rec.model.unwrap_or_else(|| DEFAULT_MODEL_NAME.into()),
            echo: Arc::new(echo),
            req: Arc::new(req),
            resp: Arc::new(resp),
        }))
    }

    /// Retention is fixed from creation; interrupted writes (`.tmp`) go after a while; then, above responses_max_mb,
    /// blobs no response uses go first, then the oldest responses (and the blobs only they used) until the store is
    /// back under 90% of the cap. Runs at startup and hourly.
    pub fn purge(&self) -> usize {
        let Some(dir) = &self.dir else { return 0 };
        let _purging = self.disk.write().unwrap_or_else(|e| e.into_inner());
        let cutoff = time_now() - self.retention_s;
        let blobs_dir = dir.join("blobs");
        let list = |d: &Path| -> Vec<(PathBuf, String, f64, u64)> {
            fs::read_dir(d)
                .map(|it| {
                    it.filter_map(Result::ok)
                        .filter_map(|e| {
                            let p = e.path();
                            let name = p.file_name()?.to_str()?.to_string();
                            let m = fs::metadata(&p).ok()?;
                            let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
                            Some((p, name, mtime, m.len()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let young = |mtime: f64| time_now() - mtime < GRACE.as_secs_f64();
        let mut removed = 0;
        let mut responses = vec![];
        let mut blobs: HashMap<String, (PathBuf, f64, u64)> = HashMap::new();
        let mut total: u64 = 0;
        for (in_blobs, (f, name, mtime, size)) in
            list(dir).into_iter().map(|x| (false, x)).chain(list(&blobs_dir).into_iter().map(|x| (true, x)))
        {
            if name.ends_with(".tmp") {
                if !young(mtime) {
                    let _ = fs::remove_file(&f);
                    removed += 1;
                }
                continue;
            }
            let Some(stem) = name.strip_suffix(".json") else { continue };
            if !in_blobs && !stem.starts_with("resp_") {
                continue;
            }
            if mtime < cutoff {
                let _ = fs::remove_file(&f);
                removed += 1;
                continue;
            }
            total += size;
            if in_blobs {
                blobs.insert(stem.to_string(), (f, mtime, size));
            } else {
                responses.push((mtime, size, f, stem.to_string()));
            }
        }
        if total > self.max_bytes {
            let target = (self.max_bytes as f64 * 0.9) as u64;
            // mark: which blobs each response uses
            let mut uses: HashMap<String, usize> = HashMap::new();
            let mut refs_of: HashMap<String, Vec<String>> = HashMap::new();
            for (_, _, f, stem) in &responses {
                let hashes: Vec<String> = read_json::<Refs>(f).map(|r| r.hashes().collect()).unwrap_or_default();
                for h in &hashes {
                    *uses.entry(h.clone()).or_default() += 1;
                }
                refs_of.insert(stem.clone(), hashes);
            }
            // sweep: a blob no response uses (and no write in progress may be about to use)
            let sweep = |hash: &str, uses: &HashMap<String, usize>, blobs: &mut HashMap<String, (PathBuf, f64, u64)>, total: &mut u64| {
                let Some((f, mtime, size)) = blobs.get(hash) else { return false };
                if uses.get(hash).copied().unwrap_or(0) > 0 || young(*mtime) || fs::remove_file(f).is_err() {
                    return false;
                }
                *total -= size;
                blobs.remove(hash);
                true
            };
            for hash in blobs.keys().cloned().collect::<Vec<_>>() {
                removed += usize::from(sweep(&hash, &uses, &mut blobs, &mut total));
            }
            responses.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, size, f, stem) in responses {
                if total <= target {
                    break;
                }
                let _ = fs::remove_file(&f);
                {
                    let mut m = self.mem();
                    if let Some(e) = m.entries.shift_remove(&stem) {
                        m.bytes -= e.bytes;
                    }
                }
                total -= size;
                removed += 1;
                for h in refs_of.remove(&stem).unwrap_or_default() {
                    if let Some(n) = uses.get_mut(&h) {
                        *n = n.saturating_sub(1);
                    }
                    removed += usize::from(sweep(&h, &uses, &mut blobs, &mut total));
                }
            }
            tracing::warn!("responses store: above {} MB, removed unused blobs and the oldest responses", self.max_bytes / (1024 * 1024));
        }
        if removed > 0 {
            tracing::info!("responses store: purged {removed} files");
        }
        removed
    }
}
