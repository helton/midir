//! The Responses API state: an in-memory cache plus, when a directory is configured, one small JSON file
//! per response holding only what that response added (parent id, new input turns, output); system prompts and tool
//! lists are stored once per distinct value (by content hash). Same on-disk format as the Python implementation (0.0.x), so chains survive
//! switching implementations. Files are owner-only (0600, folders 0700); purged at startup and hourly.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::canonical::{CanonicalRequest, CanonicalResponse, ToolCall, ToolResult, ToolSpec, Turn, Usage};
use crate::py::json as pyjson;
use crate::py::text;

const LOG: &str = "midir.store";
const MEMORY_MAX: usize = 500;

pub type Entry = (f64, Arc<CanonicalRequest>, Arc<CanonicalResponse>);

pub struct ResponseStore {
    pub dir: Option<PathBuf>,
    retention_s: f64,
    max_bytes: u64,
    memory: Mutex<IndexMap<String, Entry>>,
}

pub fn time_now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn g(v: f64) -> String {
    // Python's %g
    if v.fract() == 0.0 && v.abs() < 1e16 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

fn touch(path: &Path) -> std::io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: a valid NUL-terminated path and a null times pointer (= now).
    let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), std::ptr::null(), 0) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn valid_id(rid: &str) -> bool {
    rid.len() == 29 && rid.starts_with("resp_") && rid[5..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

type Fail = String;

fn field<'a>(d: &'a Map<String, Value>, k: &str) -> Result<&'a Value, Fail> {
    d.get(k).ok_or_else(|| format!("KeyError({})", text::repr_str(k)))
}

fn check_keys(d: &Map<String, Value>, allowed: &[&str], required: &[&str], what: &str) -> Result<(), Fail> {
    if let Some(k) = d.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("TypeError(\"{what}.__init__() got an unexpected keyword argument '{k}'\")"));
    }
    if let Some(k) = required.iter().find(|k| !d.contains_key(**k)) {
        return Err(format!("TypeError(\"{what}.__init__() missing 1 required positional argument: '{k}'\")"));
    }
    Ok(())
}

fn as_map(v: &Value) -> Result<&Map<String, Value>, Fail> {
    v.as_object().ok_or_else(|| format!("TypeError('argument after ** must be a mapping, not {}')", text::type_name(v)))
}

fn as_list(v: &Value) -> Result<&Vec<Value>, Fail> {
    v.as_array().ok_or_else(|| format!("TypeError(\"'{}' object is not iterable\")", text::type_name(v)))
}

fn call_in(v: &Value) -> Result<ToolCall, Fail> {
    let d = as_map(v)?;
    check_keys(d, &["id", "name", "arguments"], &["id", "name", "arguments"], "ToolCall")?;
    Ok(ToolCall { id: text::str_of(&d["id"]), name: text::str_of(&d["name"]), arguments: d["arguments"].clone() })
}

fn result_in(v: &Value) -> Result<ToolResult, Fail> {
    let d = as_map(v)?;
    check_keys(d, &["call_id", "content", "name", "is_error"], &["call_id", "content"], "ToolResult")?;
    Ok(ToolResult {
        call_id: text::str_of(&d["call_id"]),
        content: text::str_of(&d["content"]),
        name: d.get("name").map(text::str_of).unwrap_or_default(),
        is_error: d.get("is_error").map_or(false, text::truthy),
    })
}

fn spec_in(v: &Value) -> Result<ToolSpec, Fail> {
    let d = as_map(v)?;
    check_keys(d, &["name", "description", "parameters", "custom"], &["name"], "ToolSpec")?;
    Ok(ToolSpec::new(
        d.get("name"),
        d.get("description"),
        d.get("parameters").cloned().unwrap_or(json!({"type": "object", "properties": {}})),
        d.get("custom").map_or(false, text::truthy),
    ))
}

fn turn_in(v: &Value) -> Result<Turn, Fail> {
    let d = as_map(v)?;
    Ok(Turn {
        role: text::str_of(field(d, "role")?),
        text: text::str_of(field(d, "text")?),
        tool_calls: as_list(field(d, "tool_calls")?)?.iter().map(call_in).collect::<Result<_, _>>()?,
        tool_results: as_list(field(d, "tool_results")?)?.iter().map(result_in).collect::<Result<_, _>>()?,
        after: d.get("after").map(text::str_of).unwrap_or_default(),
    })
}

fn turn_out(t: &Turn) -> Value {
    json!({"role": t.role, "text": t.text, "tool_calls": t.tool_calls.iter().map(ToolCall::to_json).collect::<Vec<_>>(),
           "tool_results": t.tool_results.iter().map(ToolResult::to_json).collect::<Vec<_>>(), "after": t.after})
}

fn usage_in(v: &Value) -> Result<Usage, Fail> {
    let d = as_map(v)?;
    let n = |k: &str| -> Result<i64, Fail> { Ok(field(d, k)?.as_i64().unwrap_or(0)) };
    Ok(Usage { prompt_tokens: n("prompt_tokens")?, completion_tokens: n("completion_tokens")?, total_tokens: n("total_tokens")? })
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
        let setup = (|| -> std::io::Result<()> {
            fs::create_dir_all(dir.join("blobs"))?;
            for d in [dir.clone(), dir.join("blobs")] {
                fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        })();
        if let Err(e) = setup {
            crate::warn!(
                LOG,
                "responses store: cannot use {} ({e}); previous_response_id is kept in memory only (lost on restart)",
                dir.display()
            );
            return store;
        }
        crate::info!(LOG, "responses store: {} (retention {} days from creation, max {} MB)", dir.display(), g(retention_days), g(max_mb));
        store.dir = Some(dir);
        store.purge();
        store
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, IndexMap<String, Entry>> {
        self.memory.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn describe(&self) -> String {
        match &self.dir {
            Some(d) => format!("kept {} days in {}", g(self.retention_s / 86400.0), d.display()),
            None => "kept in memory only; a restart forgets them".into(),
        }
    }

    /// The telemetry session of a stored response (memory only).
    pub fn session_of(&self, rid: &str) -> Option<String> {
        self.mem().get(rid).map(|e| e.1.meta().session.clone())
    }

    fn blob(&self, data: &Value) -> std::io::Result<Value> {
        if !text::truthy(data) {
            return Ok(Value::Null);
        }
        let dir = self.dir.as_ref().expect("store dir");
        let sorted = pyjson::dumps(data, pyjson::Style { ensure_ascii: false, compact: false, sort_keys: true });
        let digest = Sha256::digest(sorted.as_bytes());
        let h = crate::canonical::hex(&digest)[..32].to_string();
        let f = dir.join("blobs").join(format!("{h}.json"));
        if f.exists() {
            touch(&f)?;
        } else {
            write_file(&f, data)?;
        }
        Ok(Value::String(h))
    }

    fn unblob(&self, h: &Value) -> Result<Value, Fail> {
        let dir = self.dir.as_ref().ok_or("no dir")?;
        let f = dir.join("blobs").join(format!("{}.json", text::str_of(h)));
        touch(&f).map_err(|e| format!("FileNotFoundError({e})"))?;
        let raw = fs::read_to_string(&f).map_err(|e| format!("OSError({e})"))?;
        pyjson::loads(&raw).map_err(|e| format!("JSONDecodeError({})", text::repr_str(&e.text)))
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
        self.load_disk(rid)
    }

    fn load_disk(&self, rid: &str) -> Option<Entry> {
        let dir = self.dir.as_ref()?;
        if !valid_id(rid) {
            return None;
        }
        match self.rebuild(dir, rid) {
            Ok(Some(entry)) => {
                self.mem().insert(rid.to_string(), entry.clone());
                Some(entry)
            }
            Ok(None) => None,
            Err(e) => {
                crate::warn!(LOG, "responses store: could not rebuild {rid}: {e}");
                None
            }
        }
    }

    fn rebuild(&self, dir: &Path, rid: &str) -> Result<Option<Entry>, Fail> {
        let mut chain: Vec<Map<String, Value>> = vec![];
        let mut cur: Option<String> = Some(rid.to_string());
        while let Some(c) = cur.filter(|c| !c.is_empty()) {
            let f = dir.join(format!("{c}.json"));
            if !f.exists() {
                if !chain.is_empty() {
                    crate::warn!(LOG, "responses store: {c} is missing from the chain of {rid} (purged?)");
                }
                return Ok(None);
            }
            let raw = fs::read_to_string(&f).map_err(|e| format!("OSError({e})"))?;
            let rec = pyjson::loads(&raw).map_err(|e| format!("JSONDecodeError({})", text::repr_str(&e.text)))?;
            let rec = as_map(&rec)?.clone();
            cur = match rec.get("parent") {
                Some(p) if text::truthy(p) => Some(text::str_of(p)),
                _ => None,
            };
            chain.push(rec);
            if chain.len() > 10000 {
                return Err("ValueError('chain too long')".into());
            }
        }
        chain.reverse();
        let mut req = CanonicalRequest::default();
        let last = chain.len() - 1;
        for (k, rec) in chain.iter().enumerate() {
            if let Some(s) = rec.get("system").filter(|s| text::truthy(s)) {
                req.system = as_list(&self.unblob(s)?)?.iter().map(text::str_of).collect();
            }
            if let Some(t) = rec.get("tools").filter(|t| text::truthy(t)) {
                req.tools = as_list(&self.unblob(t)?)?.iter().map(spec_in).collect::<Result<_, _>>()?;
            }
            for t in as_list(field(rec, "turns")?)? {
                req.turns.push(turn_in(t)?);
            }
            if k < last {
                let o = as_map(field(rec, "resp")?)?;
                let calls = as_list(field(o, "tool_calls")?)?.iter().map(call_in).collect::<Result<_, _>>()?;
                req.turns.push(Turn {
                    role: "assistant".into(),
                    text: text::str_of(field(o, "text")?),
                    tool_calls: calls,
                    tool_results: vec![],
                    after: String::new(),
                });
            }
        }
        let o = as_map(field(&chain[last], "resp")?)?;
        let resp = CanonicalResponse {
            text: text::str_of(field(o, "text")?),
            tool_calls: as_list(field(o, "tool_calls")?)?.iter().map(call_in).collect::<Result<_, _>>()?,
            finish: text::str_of(field(o, "finish")?),
            usage: usage_in(field(o, "usage")?)?,
            ..Default::default()
        };
        let ts = field(&chain[last], "ts")?.as_f64().unwrap_or(0.0);
        Ok(Some((ts, Arc::new(req), Arc::new(resp))))
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
        let r = (|| -> std::io::Result<()> {
            let (parent, prev_turns) = {
                let m = req.meta();
                (m.prev_id.clone(), m.prev_turns)
            };
            let system = self.blob(&json!(req.system))?;
            let tools = self.blob(&Value::Array(req.tools.iter().map(ToolSpec::to_json).collect()))?;
            let turns: Vec<Value> = req.turns.iter().skip(prev_turns).map(turn_out).collect();
            let record = json!({"v": 1, "id": rid, "ts": pyjson::float(now), "parent": parent, "system": system, "tools": tools, "turns": turns,
                                "resp": {"text": resp.text, "tool_calls": resp.tool_calls.iter().map(ToolCall::to_json).collect::<Vec<_>>(), "finish": resp.finish,
                                         "usage": resp.usage.to_json()}});
            write_file(&dir.join(format!("{rid}.json")), &record)
        })();
        if let Err(e) = r {
            crate::warn!(LOG, "responses store: could not persist {rid}: {e}");
        }
    }

    /// Retention is fixed from creation; then, above responses_max_mb, the oldest responses go until the store is
    /// back under 90% of the cap. Runs at startup and hourly.
    pub fn purge(&self) -> usize {
        let Some(dir) = &self.dir else { return 0 };
        let cutoff = time_now() - self.retention_s;
        let mut n = 0;
        let mut files: Vec<(f64, u64, PathBuf)> = vec![];
        let list = |d: &Path, prefix: &str| -> Vec<PathBuf> {
            fs::read_dir(d)
                .map(|it| {
                    it.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.starts_with(prefix) && n.ends_with(".json")))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut candidates = list(dir, "resp_");
        candidates.extend(list(&dir.join("blobs"), ""));
        for f in candidates {
            let Ok(st) = fs::metadata(&f) else { continue };
            let mtime = st.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64());
            if mtime < cutoff {
                let _ = fs::remove_file(&f);
                n += 1;
            } else if f.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.starts_with("resp_")) && f.parent() == Some(dir.as_path())
            {
                files.push((mtime, st.len(), f));
            }
        }
        let blobs: u64 = list(&dir.join("blobs"), "").iter().filter_map(|b| fs::metadata(b).ok()).map(|m| m.len()).sum();
        let mut total: u64 = files.iter().map(|f| f.1).sum::<u64>() + blobs;
        if total > self.max_bytes {
            files.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            for (_, sz, f) in files {
                if (total as f64) <= self.max_bytes as f64 * 0.9 {
                    break;
                }
                let _ = fs::remove_file(&f);
                if let Some(stem) = f.file_stem().and_then(|s| s.to_str()) {
                    self.mem().shift_remove(stem);
                }
                total -= sz;
                n += 1;
            }
            crate::warn!(LOG, "responses store: above {} MB, removed the oldest responses", self.max_bytes / (1024 * 1024));
        }
        if n > 0 {
            crate::info!(LOG, "responses store: purged {n} files");
        }
        n
    }
}

fn write_file(path: &Path, data: &Value) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(pyjson::dumps(data, pyjson::DEFAULT).as_bytes())?;
    }
    fs::rename(&tmp, path)
}
