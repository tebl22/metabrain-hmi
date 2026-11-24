//! Shared memory between brains: file locks, a change log, tickets and the project index.
//! Port of metabrain/memory.py. Driven by hooks, so it costs zero tokens to maintain.
use crate::util::{expand, norm, now, J};
use serde_json::{json, Value};
use sha1_smol::Sha1;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

const DEFAULT_NO_LOCK: [&str; 5] = ["ESTADO.md", "TAREAS.md", "STATUS.md", "TODO.md", "INDEX.md"];
const DEFAULT_NOTE_CMD: &str = "metabrain note {brain} \"what you did\"";

pub struct Memory {
    pub enabled: bool,
    pub root: PathBuf,
    pub roots: Vec<PathBuf>,
    pub ttl: f64,
    pub events: PathBuf,
    pub legacy_events: PathBuf,
    pub note_cmd: String,
    pub locks: PathBuf,
    pub tickets: PathBuf,
    pub shared: PathBuf,
    pub no_lock: HashSet<String>,
    io: Mutex<()>,
}

impl Memory {
    pub fn from_cfg(cfg: &Value) -> Memory {
        let m = cfg.o("memory");
        let enabled = m.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        let root = expand(
            &m.os("dir")
                .unwrap_or_else(|| "~/metabrain-memory".to_string()),
        );
        let roots: Vec<PathBuf> = m
            .arr("roots")
            .iter()
            .filter_map(|v| v.as_str())
            .map(expand)
            .collect();
        let lock_minutes = match m.get("lock_minutes") {
            Some(Value::Number(n)) => n.as_f64().unwrap_or(20.0),
            Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(20.0),
            _ => 20.0,
        };
        let ttl = (lock_minutes.trunc() as i64) as f64 * 60.0;
        let note_cmd = m
            .os("note_cmd")
            .unwrap_or_else(|| DEFAULT_NOTE_CMD.to_string());
        let no_lock: HashSet<String> = match m.get("no_lock") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_lowercase())
                .collect(),
            Some(Value::String(s)) => {
                let mut h = HashSet::new();
                h.insert(s.to_lowercase());
                h
            }
            _ => DEFAULT_NO_LOCK.iter().map(|s| s.to_lowercase()).collect(),
        };
        let mem = Memory {
            enabled,
            events: root.join("events.jsonl"),
            legacy_events: root.join("eventos.jsonl"),
            locks: root.join("locks"),
            tickets: root.join("tickets"),
            shared: root.join("shared"),
            root,
            roots,
            ttl,
            note_cmd,
            no_lock,
            io: Mutex::new(()),
        };
        if mem.enabled {
            for d in [&mem.locks, &mem.tickets, &mem.shared] {
                let _ = fs::create_dir_all(d);
            }
        }
        mem
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.io.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ----- log -----
    pub fn note(
        &self,
        brain: &str,
        text: &str,
        path: Option<&str>,
        session: Option<&str>,
        kind: &str,
    ) -> Value {
        let file = path.filter(|p| !p.is_empty()).map(abspath);
        let e = json!({"ts": now(), "brain": brain, "kind": kind, "text": text, "file": file,
                       "session": session});
        let _g = self.guard();
        if let Some(parent) = self.events.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events)
        {
            let line = format!(
                "{}\n",
                serde_json::to_string(&e).unwrap_or_else(|_| "{}".into())
            );
            let _ = f.write_all(line.as_bytes());
        }
        e
    }

    pub fn recent(&self, hours: f64, limit: usize) -> Vec<Value> {
        let lim = now() - hours * 3600.0;
        let mut out: Vec<Value> = Vec::new();
        for path in [&self.events, &self.legacy_events] {
            for line in tail_lines(path, 400_000) {
                let Ok(mut e) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if e.get("cerebro").is_some() {
                    // older Spanish log format (cerebros.py)
                    let ts = e.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let brain = e.get("cerebro").cloned().unwrap_or(Value::Null);
                    let text = e
                        .get("texto")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let file = e.get("archivo").cloned().unwrap_or(Value::Null);
                    e = json!({"ts": ts, "brain": brain, "text": text, "file": file, "kind": "note"});
                }
                if val_ts(&e) >= lim {
                    out.push(e);
                }
            }
        }
        out.sort_by(|a, b| {
            val_ts(a)
                .partial_cmp(&val_ts(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if limit == 0 {
            return out;
        }
        let skip = out.len().saturating_sub(limit);
        out.split_off(skip)
    }

    // ----- locks -----
    fn lock_file(&self, path: &str) -> PathBuf {
        let key = norm(Path::new(&abspath(path)));
        let mut h = Sha1::new();
        h.update(key.as_bytes());
        let hex = h.digest().to_string();
        let short: String = hex.chars().take(16).collect();
        self.locks.join(format!("{short}.json"))
    }

    pub fn active_locks(&self) -> Vec<Value> {
        let _g = self.guard();
        let mut out = Vec::new();
        for f in glob_files(&self.locks.join("*.json")) {
            if let Some(c) = crate::util::read_json(&f) {
                if let Some(ts) = c.get("ts").and_then(|v| v.as_f64()) {
                    if now() - ts > self.ttl {
                        let _ = fs::remove_file(&f);
                    } else {
                        out.push(c);
                    }
                }
            }
        }
        out
    }

    /// -> None if granted, or the lock that blocks it.
    pub fn claim(&self, path: &str, brain: &str, session: Option<&str>) -> Option<Value> {
        if self.no_lock.contains(&basename(path).to_lowercase()) {
            return None;
        }
        let k = self.lock_file(path);
        let _g = self.guard();
        let _flock = crate::util::FileLock::acquire(&k, 2000);
        let mut since = now();
        if let Some(c) = crate::util::read_json(&k) {
            if let Some(ts) = c.get("ts").and_then(|v| v.as_f64()) {
                if c.get("brain").is_some() {
                    let owner = c.get("brain").and_then(|v| v.as_str());
                    if now() - ts < self.ttl && owner != Some(brain) {
                        return Some(c);
                    }
                    if session_eq(c.get("session"), session) {
                        if let Some(s) = c.get("since").and_then(|v| v.as_f64()) {
                            since = s;
                        }
                    }
                }
            }
        }
        let v = json!({"file": abspath(path), "brain": brain, "session": session,
                       "since": since, "ts": now()});
        let _ = crate::util::write_json(&k, &v);
        None
    }

    pub fn release(&self, session: Option<&str>) {
        let _g = self.guard();
        let _flock = crate::util::FileLock::acquire(&self.locks.join("_release"), 2000);
        for f in glob_files(&self.locks.join("*.json")) {
            if let Some(c) = crate::util::read_json(&f) {
                if session_eq(c.get("session"), session) {
                    let _ = fs::remove_file(&f);
                }
            }
        }
    }

    // ----- tickets -----
    pub fn tickets_for(&self, brain: &str) -> Vec<String> {
        let mut files: Vec<(f64, PathBuf)> = glob_files(&self.tickets.join("*.md"))
            .into_iter()
            .map(|p| {
                let m = fs::metadata(&p)
                    .ok()
                    .and_then(|md| md.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                (m, p)
            })
            .collect();
        files.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let needle_to = format!("to:{}", brain.to_lowercase());
        let needle_para = format!("para:{}", brain.to_lowercase());
        let mut out = Vec::new();
        for (_, f) in files {
            let Ok(file) = File::open(&f) else { continue };
            let mut buf = Vec::new();
            if BufReader::new(file).read_until(b'\n', &mut buf).is_err() {
                continue;
            }
            let head = String::from_utf8_lossy(&buf)
                .to_lowercase()
                .replace(' ', "");
            if head.contains(&needle_to) || head.contains(&needle_para) {
                out.push(
                    f.file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                );
            }
        }
        out
    }

    // ----- context injected at session start -----
    pub fn briefing(&self, brain: &str, name: &str) -> String {
        let mut l: Vec<String> = vec![format!(
            "[METABRAIN] You are {name} (brain '{brain}'). Shared memory: {} \
             (INDEX.md map, tickets/ requests, shared/ hand-offs).",
            self.root.display()
        )];
        let t = self.tickets_for(brain);
        if !t.is_empty() {
            let joined = t.iter().take(6).cloned().collect::<Vec<_>>().join("; ");
            l.push(format!("Tickets for you ({}): {}", t.len(), joined));
        }
        let others: Vec<Value> = self
            .active_locks()
            .into_iter()
            .filter(|c| c.get("brain").and_then(|v| v.as_str()).unwrap_or("") != brain)
            .collect();
        if !others.is_empty() {
            let items: Vec<String> = others
                .iter()
                .take(8)
                .map(|c| {
                    let file = c.get("file").and_then(|v| v.as_str()).unwrap_or("");
                    let b = c.get("brain").and_then(|v| v.as_str()).unwrap_or("");
                    format!("{} ({})", basename(file), b)
                })
                .collect();
            l.push(format!(
                "Files another brain is editing (do not touch): {}",
                items.join("; ")
            ));
        }
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut last: Vec<String> = Vec::new();
        for e in self.recent(24.0, 300).iter().rev() {
            let b = e
                .get("brain")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if b == brain {
                continue;
            }
            let file = e.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let text = e.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let key_file = if file.is_empty() {
                text.to_string()
            } else {
                file.to_string()
            };
            if !seen.insert((b.clone(), key_file)) {
                continue;
            }
            let what = if text.is_empty() {
                format!("edited {}", basename(file))
            } else {
                text.to_string()
            };
            last.push(format!(
                "- {} {}: {}",
                hhmm(val_ts(e)),
                b,
                chars_take(&what, 140)
            ));
            if last.len() >= 8 {
                break;
            }
        }
        if !last.is_empty() {
            l.push("Latest from other brains (24 h):".to_string());
            l.extend(last);
        }
        l.push(format!(
            "When you finish something important: {}",
            self.note_cmd.replace("{brain}", brain)
        ));
        l.join("\n")
    }

    pub fn write_index(&self) -> std::io::Result<()> {
        let _g = self.guard();
        let mut l: Vec<String> = vec![
            "# INDEX (automatic, do not edit)".to_string(),
            format!("Updated: {}", chrono::Local::now().format("%Y-%m-%d %H:%M")),
            String::new(),
        ];
        let skip: HashSet<String> = [
            ".git",
            "node_modules",
            "__pycache__",
            ".venv",
            "venv",
            "build",
            "dist",
            "frames",
            "cache",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let binary = [
            ".png", ".jpg", ".jpeg", ".mp4", ".wav", ".mp3", ".bin", ".zip", ".exr", ".pyc",
            ".class", ".lock",
        ];
        for pattern in &self.roots {
            for r in glob_files(pattern).into_iter().filter(|p| p.is_dir()) {
                let name = r
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| r.to_string_lossy().to_string());
                l.push(format!("## {}  ({})", name, r.display()));
                let mut recent: Vec<(f64, String)> = Vec::new();
                let mut n = 0usize;
                walk(&r, &r, 0, &skip, &binary, &mut n, &mut recent);
                recent.sort_by(|a, b| {
                    b.0.partial_cmp(&a.0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.1.cmp(&a.1))
                });
                if !recent.is_empty() {
                    let top: Vec<String> = recent.iter().take(8).map(|(_, p)| p.clone()).collect();
                    l.push(format!("Recently changed: {}", top.join(", ")));
                }
                l.push(String::new());
            }
        }
        fs::create_dir_all(&self.root)?;
        let tmp = self.root.join("INDEX.tmp");
        fs::write(&tmp, l.join("\n") + "\n")?;
        fs::rename(&tmp, self.root.join("INDEX.md"))?;
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

fn val_ts(v: &Value) -> f64 {
    v.get("ts").and_then(|x| x.as_f64()).unwrap_or(0.0)
}

fn session_eq(v: Option<&Value>, session: Option<&str>) -> bool {
    match (v, session) {
        (None, None) => true,
        (Some(Value::Null), None) => true,
        (Some(Value::String(s)), Some(t)) => s.as_str() == t,
        _ => false,
    }
}

fn chars_take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn hhmm(ts: f64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_string())
}

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Absolute, lexically normalised path (does not touch the disk).
fn abspath(p: &str) -> String {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    normalize(&abs).to_string_lossy().to_string()
}

fn glob_files(pattern: &Path) -> Vec<PathBuf> {
    let pat = pattern.to_string_lossy().to_string();
    match glob::glob(&pat) {
        Ok(it) => {
            let mut v: Vec<PathBuf> = it.filter_map(|r| r.ok()).collect();
            v.sort();
            v
        }
        Err(_) => Vec::new(),
    }
}

/// Last `max` bytes of a file, split in lines (lossy UTF-8; partial first line is fine).
fn tail_lines(path: &Path, max: u64) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max {
        let _ = f.seek(SeekFrom::Start(len - max));
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf)
        .lines()
        .map(|s| s.to_string())
        .collect()
}

fn walk(
    dir: &Path,
    root: &Path,
    depth: usize,
    skip: &HashSet<String>,
    binary: &[&str],
    n: &mut usize,
    recent: &mut Vec<(f64, String)>,
) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            let lower = name.to_lowercase();
            if !skip.contains(&lower) && !name.starts_with('.') {
                subdirs.push(ent.path());
            }
        } else {
            *n += 1;
            let lower = name.to_lowercase();
            if !binary.iter().any(|b| lower.ends_with(*b)) {
                let p = ent.path();
                let mtime = ent
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let rel = p
                    .strip_prefix(root)
                    .map(|x| x.to_string_lossy().to_string())
                    .unwrap_or_else(|_| name.to_string());
                recent.push((mtime, rel));
            }
        }
    }
    if *n > 6000 {
        return true;
    }
    if depth >= 4 {
        return false;
    }
    for d in subdirs {
        if walk(&d, root, depth + 1, skip, binary, n, recent) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::now;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("mb_mem_{}_{}_{}", tag, std::process::id(), n));
        let _ = fs::create_dir_all(&p);
        p
    }

    fn cfg_for(dir: &Path) -> Value {
        json!({"memory": {"dir": dir.to_string_lossy(), "lock_minutes": 20}})
    }

    #[test]
    fn config_and_dirs() {
        let d = tmpdir("cfg");
        let cfg = json!({"memory": {"dir": d.to_string_lossy(), "lock_minutes": 5,
                                    "roots": [d.join("proj").to_string_lossy()]}});
        let m = Memory::from_cfg(&cfg);
        assert!(m.enabled);
        assert_eq!(m.ttl, 300.0);
        assert!(m.locks.is_dir() && m.tickets.is_dir() && m.shared.is_dir());
        assert!(m.no_lock.contains("estado.md"));
        assert_eq!(m.roots.len(), 1);
        assert_eq!(m.events, m.root.join("events.jsonl"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn notes_and_legacy_merge() {
        let d = tmpdir("rec");
        let m = Memory::from_cfg(&cfg_for(&d));
        m.note("alpha", "did work", Some("src/a.rs"), Some("s1"), "edit");
        let line =
            json!({"ts": now() - 10.0, "cerebro": "beta", "texto": "hola", "archivo": "b.py"})
                .to_string();
        fs::write(&m.legacy_events, format!("{}\n", line)).unwrap();
        let r = m.recent(24.0, 300);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].get("brain").and_then(|v| v.as_str()), Some("beta"));
        assert_eq!(r[0].get("kind").and_then(|v| v.as_str()), Some("note"));
        assert_eq!(r[0].get("file").and_then(|v| v.as_str()), Some("b.py"));
        assert_eq!(r[1].get("brain").and_then(|v| v.as_str()), Some("alpha"));
        assert_eq!(r[1].get("text").and_then(|v| v.as_str()), Some("did work"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn claim_and_release() {
        let d = tmpdir("lock");
        let m = Memory::from_cfg(&cfg_for(&d));
        let p = d.join("x.txt").to_string_lossy().to_string();
        assert!(m.claim(&p, "a", Some("s1")).is_none());
        let block = m.claim(&p, "b", Some("s2")).expect("blocked lock");
        assert_eq!(block.get("brain").and_then(|v| v.as_str()), Some("a"));
        assert!(m.claim(&p, "a", Some("s1")).is_none());
        assert_eq!(m.active_locks().len(), 1);
        m.release(Some("s1"));
        assert!(m.active_locks().is_empty());
        assert!(m.claim(&p, "b", Some("s2")).is_none());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn no_lock_basenames() {
        let d = tmpdir("nolock");
        let m = Memory::from_cfg(&cfg_for(&d));
        assert!(m.claim("C:/proj/ESTADO.md", "a", None).is_none());
        assert!(m.active_locks().is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn tickets_match_brain() {
        let d = tmpdir("tk");
        let m = Memory::from_cfg(&cfg_for(&d));
        fs::write(m.tickets.join("t1.md"), "TO: Alpha\nbody\n").unwrap();
        fs::write(m.tickets.join("t2.md"), "para:alpha: fix\n").unwrap();
        fs::write(m.tickets.join("t3.md"), "to:beta\n").unwrap();
        let t = m.tickets_for("Alpha");
        assert_eq!(t.len(), 2);
        assert!(t.contains(&"t1.md".to_string()));
        assert!(t.contains(&"t2.md".to_string()));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn briefing_has_sections() {
        let d = tmpdir("br");
        let m = Memory::from_cfg(&cfg_for(&d));
        m.note("beta", "touched c.py", Some("c.py"), Some("s2"), "edit");
        let b = m.briefing("alpha", "Alice");
        assert!(b.contains("You are Alice (brain 'alpha')"));
        assert!(b.contains("Latest from other brains"));
        assert!(b.contains("beta: touched c.py"));
        assert!(b.contains("metabrain note alpha"));
        let _ = fs::remove_dir_all(&d);
    }
}
