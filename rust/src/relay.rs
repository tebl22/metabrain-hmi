//! Continuous relay: keep a per-project handoff fresh from the session log (no quota spent)
//! and hand it to a different brain when it opens the same project.

use crate::handoff::{collect_with, render};
use crate::util::{data_dir, log, norm, now, read_json, write_json, J};
use serde_json::{json, Value};
use sha1_smol::Sha1;
use std::fs;
use std::path::{Path, PathBuf};

const THROTTLE_SECS: f64 = 90.0;
const INJECT_MAX_AGE_SECS: f64 = 8.0 * 3600.0;
const STATE_MAX_AGE_SECS: f64 = 3.0 * 86400.0;
const INJECT_MAX_CHARS: usize = 1600;

fn relay_dir() -> PathBuf {
    let d = data_dir().join("relay");
    let _ = fs::create_dir_all(&d);
    d
}

fn project_name(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "root".to_string()
    } else {
        out
    }
}

fn key_for(cwd: &str) -> String {
    let normalized = norm(Path::new(cwd)).to_lowercase();
    let mut h = Sha1::new();
    h.update(normalized.as_bytes());
    let hex = h.digest().to_string();
    let short: String = hex.chars().take(10).collect();
    format!("{}_{}", short, project_name(cwd))
}

fn object_or_empty(v: Option<Value>) -> Value {
    match v {
        Some(v) if v.is_object() => v,
        _ => json!({}),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn relative_path(file: &str, base: &str) -> String {
    let f = file.trim();
    if f.is_empty() {
        return String::new();
    }
    let b = base.trim_end_matches(['/', '\\']);
    if !b.is_empty() {
        if let Some(rest) = f.strip_prefix(b) {
            let rest = rest.trim_start_matches(['/', '\\']);
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
        if cfg!(windows) {
            let mut f_chars = f.chars();
            let mut ok = true;
            for bc in b.chars() {
                match f_chars.next() {
                    Some(fc) if fc.to_lowercase().eq(bc.to_lowercase()) => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                let rest: String = f_chars.collect();
                let rest = rest.trim_start_matches(['/', '\\']);
                if !rest.is_empty() {
                    return rest.to_string();
                }
            }
        }
    }
    f.to_string()
}

fn value_line(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o
            .get("text")
            .or_else(|| o.get("summary"))
            .or_else(|| o.get("action"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn value_path(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o
            .get("path")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn command_entry(v: &Value) -> Option<(String, bool)> {
    match v {
        Value::String(s) => Some((s.clone(), false)),
        Value::Object(o) => {
            let s = o
                .get("cmd")
                .or_else(|| o.get("command"))
                .or_else(|| o.get("text"))
                .and_then(|x| x.as_str())?;
            let failed = o.get("failed").and_then(|x| x.as_bool()).unwrap_or(false)
                || o.get("exit_code")
                    .and_then(|x| x.as_i64())
                    .map(|c| c != 0)
                    .unwrap_or(false);
            Some((s.to_string(), failed))
        }
        _ => None,
    }
}

/// Build the compact handoff note (<= INJECT_MAX_CHARS chars).
pub fn compact(
    info: &Value,
    brain_name: &str,
    project: &str,
    lang: &str,
    full_path: &str,
    hhmm: &str,
) -> String {
    let es = lang == "es";
    let title = if es {
        format!("## Traspaso · {} · de {} · {}", project, brain_name, hhmm)
    } else {
        format!("## Handoff · {} · from {} · {}", project, brain_name, hhmm)
    };

    let task = truncate_chars(&one_line(&info.s("task")), 160);
    let status = {
        let raw = info
            .os("status")
            .or_else(|| info.os("last_result"))
            .or_else(|| info.os("result"))
            .or_else(|| {
                info.arr("assistant")
                    .into_iter()
                    .rev()
                    .find_map(|v| value_line(&v))
            })
            .unwrap_or_default();
        let s = truncate_chars(&one_line(&raw), 200);
        if s.is_empty() {
            "—".to_string()
        } else {
            s
        }
    };

    let mut done: Vec<String> = Vec::new();
    for k in ["done", "actions", "steps", "history", "assistant"] {
        for v in info.arr(k) {
            if let Some(s) = value_line(&v) {
                let s = one_line(&s);
                if !s.is_empty() && !done.contains(&s) {
                    done.push(s);
                }
            }
        }
        if !done.is_empty() {
            break;
        }
    }
    done.reverse(); // newest first
    done.truncate(5);
    done = done.into_iter().map(|s| truncate_chars(&s, 120)).collect();

    let base = info.os("cwd").unwrap_or_default();
    let mut files: Vec<String> = Vec::new();
    for v in info.arr("files") {
        if let Some(s) = value_path(&v) {
            let rel = relative_path(&s, &base);
            if !rel.is_empty() && !files.contains(&rel) {
                files.push(rel);
            }
        }
    }
    files.truncate(12);

    let mut cmds_all: Vec<(String, bool)> = Vec::new();
    for v in info.arr("commands") {
        if let Some((s, failed)) = command_entry(&v) {
            let s = truncate_chars(&one_line(&s), 100);
            if !s.is_empty() {
                cmds_all.push((s, failed));
            }
        }
    }
    let has_failed = cmds_all.iter().any(|(_, f)| *f);
    let mut cmds: Vec<String> = Vec::new();
    for (s, f) in cmds_all.iter().rev() {
        if has_failed && !*f {
            continue;
        }
        if !cmds.contains(s) {
            cmds.push(s.clone());
            if cmds.len() >= 3 {
                break;
            }
        }
    }

    let next = {
        let raw = info
            .os("next")
            .or_else(|| info.os("next_step"))
            .or_else(|| info.os("pending"))
            .or_else(|| info.os("last_user"))
            .unwrap_or_default();
        let s = truncate_chars(&one_line(&raw), 200);
        if s.is_empty() {
            "—".to_string()
        } else {
            s
        }
    };

    let objetivo_label = if es { "Objetivo" } else { "Goal" };
    let estado_label = if es { "Estado" } else { "Status" };
    let hecho_label = if es { "Hecho" } else { "Done" };
    let archivos_label = if es { "Archivos" } else { "Files" };
    let comandos_label = if es { "Comandos clave" } else { "Key commands" };
    let siguiente_label = if es { "Siguiente paso" } else { "Next step" };
    let detalle_label = if es {
        "Detalle completo"
    } else {
        "Full detail"
    };

    let build = |done_n: usize, files_n: usize, cmds_v: &[String]| -> String {
        let mut l: Vec<String> = Vec::new();
        l.push(title.clone());
        l.push(format!(
            "{}: {}",
            objetivo_label,
            if task.is_empty() {
                "—"
            } else {
                task.as_str()
            }
        ));
        l.push(format!("{}: {}", estado_label, status));
        if done_n == 0 {
            l.push(format!("{}: —", hecho_label));
        } else {
            l.push(format!("{}:", hecho_label));
            for d in done.iter().take(done_n) {
                l.push(format!("- {}", d));
            }
        }
        let files_part: Vec<String> = files.iter().take(files_n).cloned().collect();
        l.push(format!(
            "{}: {}",
            archivos_label,
            if files_part.is_empty() {
                "—".to_string()
            } else {
                files_part.join(", ")
            }
        ));
        l.push(format!(
            "{}: {}",
            comandos_label,
            if cmds_v.is_empty() {
                "—".to_string()
            } else {
                cmds_v.join(" · ")
            }
        ));
        l.push(format!("{}: {}", siguiente_label, next));
        l.push(format!("{}: {}", detalle_label, full_path));
        l.join("\n")
    };

    let mut done_len = done.len();
    let mut files_len = files.len();
    loop {
        let s = build(done_len, files_len, &cmds);
        if s.chars().count() <= INJECT_MAX_CHARS {
            return s;
        }
        if done_len > 0 {
            done_len -= 1;
        } else if files_len > 0 {
            files_len -= 1;
        } else {
            break;
        }
    }

    let mut cmds_len = cmds.len();
    while cmds_len > 0 {
        cmds_len -= 1;
        let s = build(done_len, files_len, &cmds[..cmds_len]);
        if s.chars().count() <= INJECT_MAX_CHARS {
            return s;
        }
    }

    let s = build(done_len, files_len, &[]);
    if s.chars().count() > INJECT_MAX_CHARS {
        let mut t: String = s.chars().take(INJECT_MAX_CHARS.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        s
    }
}

pub fn update(
    transcript: &Path,
    cwd: &str,
    session_id: &str,
    brain_id: &str,
    brain_name: &str,
    lang: &str,
    force: bool,
) -> Option<PathBuf> {
    let relay = relay_dir();
    let state_path = relay.join("state.json");
    let state = object_or_empty(read_json(&state_path));

    if !force {
        if let Some(ts) = state.get(session_id).and_then(|v| v.as_f64()) {
            if now() - ts < THROTTLE_SECS {
                return None;
            }
        }
    }

    let mut info = collect_with(transcript, 60_000);
    if info.get("cwd").is_none() {
        if let Some(obj) = info.as_object_mut() {
            obj.insert("cwd".to_string(), json!(cwd));
        }
    }
    let task = info.s("task");
    let files = info.arr("files");
    let commands = info.arr("commands");
    if task.is_empty() && files.is_empty() && commands.is_empty() {
        return None;
    }

    let rendered = render(&info, brain_name, "", "relevo continuo", lang);
    let stamp = chrono::Local::now().format("%H:%M").to_string();
    let notice = if lang == "es" {
        format!("> Traspaso automático de MetaBrain. Actualizado {stamp}.")
    } else {
        format!("> Automatic MetaBrain handoff. Updated {stamp}.")
    };
    let full_md = format!("<!-- metabrain relay -->\n{notice}\n\n{rendered}");

    let key = key_for(cwd);
    let full_path = relay.join(format!("{key}.full.md"));
    if fs::write(&full_path, full_md.as_bytes()).is_err() {
        log("relay: failed to write full handoff markdown");
        return None;
    }

    let full_path_s = full_path.to_string_lossy().to_string();
    let compact_md = compact(
        &info,
        brain_name,
        &project_name(cwd),
        lang,
        &full_path_s,
        &stamp,
    );
    let md_path = relay.join(format!("{key}.md"));
    if fs::write(&md_path, compact_md.as_bytes()).is_err() {
        log("relay: failed to write handoff markdown");
        return None;
    }

    let index_path = relay.join("index.json");
    let mut index = object_or_empty(read_json(&index_path));
    let old = index.get(&key).cloned().unwrap_or(Value::Null);
    let same_brain = old.get("brain_id").and_then(|v| v.as_str()) == Some(brain_id);
    let consumed = if same_brain {
        old.get("consumed")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}))
    } else {
        json!({})
    };

    let now_ts = now();
    let entry = json!({
        "cwd": cwd,
        "brain_id": brain_id,
        "brain_name": brain_name,
        "session_id": session_id,
        "ts": now_ts,
        "path": md_path.to_string_lossy().to_string(),
        "full_path": full_path_s,
        "files": files.len(),
        "consumed": consumed
    });

    {
        let obj = index.as_object_mut()?;
        obj.insert(key, entry);
    }

    if write_json(&index_path, &index).is_err() {
        log("relay: failed to write index");
        return None;
    }

    let mut st = state;
    if !st.is_object() {
        st = json!({});
    }
    if let Some(obj) = st.as_object_mut() {
        obj.insert(session_id.to_string(), json!(now_ts));
        let cutoff = now_ts - STATE_MAX_AGE_SECS;
        obj.retain(|_k, v| v.as_f64().map(|x| x >= cutoff).unwrap_or(false));
    }
    let _ = write_json(&state_path, &st);

    Some(md_path)
}

pub fn inject(cwd: &str, brain_id: &str, lang: &str) -> Option<String> {
    let _ = lang;
    let relay = relay_dir();
    let index_path = relay.join("index.json");
    let mut index = object_or_empty(read_json(&index_path));
    let key = key_for(cwd);

    let entry = index.get(&key)?.clone();
    let stored_brain = entry.s("brain_id");
    if stored_brain == brain_id {
        return None;
    }

    let ts = entry.f("ts")?;
    let age = now() - ts;
    if age < 0.0 || age >= INJECT_MAX_AGE_SECS {
        return None;
    }

    if entry
        .get("consumed")
        .and_then(|v| v.as_object())
        .map(|c| c.contains_key(brain_id))
        .unwrap_or(false)
    {
        return None;
    }

    let path = match entry.os("path") {
        Some(p) => PathBuf::from(p),
        None => relay.join(format!("{key}.md")),
    };
    let raw = fs::read_to_string(&path).ok()?;
    let text = if raw.chars().count() > INJECT_MAX_CHARS {
        let mut t: String = raw
            .chars()
            .take(INJECT_MAX_CHARS.saturating_sub(1))
            .collect();
        t.push('…');
        t
    } else {
        raw
    };

    {
        let obj = index.get_mut(&key).and_then(|v| v.as_object_mut())?;
        if !obj.get("consumed").map(|v| v.is_object()).unwrap_or(false) {
            obj.insert("consumed".to_string(), json!({}));
        }
        {
            let consumed = obj.get_mut("consumed").and_then(|v| v.as_object_mut())?;
            consumed.insert(brain_id.to_string(), json!(now()));
        }
    }

    if write_json(&index_path, &index).is_err() {
        log("relay: failed to persist injected relay");
        return None;
    }

    Some(text)
}

pub fn latest(limit: usize) -> Value {
    let relay = relay_dir();
    let index = object_or_empty(read_json(&relay.join("index.json")));
    let now_ts = now();
    let mut items: Vec<Value> = Vec::new();

    if let Some(obj) = index.as_object() {
        for (key, entry) in obj {
            if !entry.is_object() {
                continue;
            }
            let cwd = entry.s("cwd");
            let ts = entry.f("ts").unwrap_or(0.0);
            let age = (now_ts - ts).max(0.0) / 60.0;
            let consumed: Vec<Value> = entry
                .get("consumed")
                .and_then(|v| v.as_object())
                .map(|c| c.keys().cloned().map(Value::String).collect())
                .unwrap_or_default();
            let files = entry.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
            items.push(json!({
                "key": key,
                "cwd": cwd,
                "project": project_name(&cwd),
                "brain_id": entry.s("brain_id"),
                "brain_name": entry.s("brain_name"),
                "ts": ts,
                "age_min": age.floor() as i64,
                "files": files,
                "consumed": consumed,
                "path": entry.s("path")
            }));
        }
    }

    items.sort_by(|a, b| {
        let ta = a.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let tb = b.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);
        tb.partial_cmp(&ta).unwrap_or(std::cmp::Ordering::Equal)
    });

    if limit > 0 {
        items.truncate(limit);
    } else {
        items.clear();
    }
    Value::Array(items)
}

pub fn read(key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let path = relay_dir().join(format!("{key}.md"));
    fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::read_json;
    use serde_json::json;
    use std::fs;

    fn unique_home(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d =
            std::env::temp_dir().join(format!("mb_relay_{}_{}_{}", tag, std::process::id(), nanos));
        let _ = fs::create_dir_all(&d);
        d
    }

    fn setup(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let home = unique_home(tag);
        std::env::set_var("METABRAIN_HOME", &home);
        let cwd = home.join("proj");
        let _ = fs::create_dir_all(&cwd);
        let transcript = home.join("session.jsonl");
        let cwd_s = cwd.to_string_lossy().to_string();
        let lines = [
            json!({
                "type": "user",
                "cwd": cwd_s,
                "message": {"role": "user", "content": "implementa X"}
            })
            .to_string(),
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "name": "Edit",
                        "input": {"file_path": format!("{}/a.rs", cwd_s)}
                    }]
                }
            })
            .to_string(),
        ];
        let _ = fs::write(&transcript, lines.join("\n") + "\n");
        (home, cwd, transcript)
    }

    #[test]
    fn update_writes_and_throttles() {
        let _g = crate::util::test_env_lock();
        let (home, cwd, transcript) = setup("update");
        let cwd_s = cwd.to_string_lossy().to_string();
        let key = key_for(&cwd_s);

        let md = update(&transcript, &cwd_s, "s1", "b1", "Brain One", "es", false)
            .expect("first update should write");
        assert!(md.is_file());

        let full = home.join("relay").join(format!("{}.full.md", key));
        assert!(full.is_file());

        let index = read_json(&home.join("relay").join("index.json")).expect("index");
        let entry = index.get(&key).expect("entry");
        assert_eq!(entry.s("brain_id"), "b1");
        assert_eq!(entry.s("brain_name"), "Brain One");
        assert_eq!(entry.get("files").and_then(|v| v.as_u64()), Some(1));
        assert!(entry
            .get("consumed")
            .and_then(|v| v.as_object())
            .map(|o| o.is_empty())
            .unwrap_or(false));

        let text = fs::read_to_string(&md).unwrap_or_default();
        assert!(text.starts_with("## Traspaso · proj · de Brain One · "));
        assert!(text.contains("Objetivo: implementa X"));
        assert!(text.contains("Detalle completo: "));
        assert!(text.contains(".full.md"));

        assert!(update(&transcript, &cwd_s, "s1", "b1", "Brain One", "es", false).is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn inject_only_for_other_brain_once() {
        let _g = crate::util::test_env_lock();
        let (home, cwd, transcript) = setup("inject");
        let cwd_s = cwd.to_string_lossy().to_string();

        assert!(update(&transcript, &cwd_s, "s1", "b1", "Brain One", "es", false).is_some());
        assert!(inject(&cwd_s, "b1", "es").is_none());

        let text = inject(&cwd_s, "b2", "es").expect("other brain gets relay");
        assert!(text.starts_with("## Traspaso"));
        assert!(text.contains("Objetivo: implementa X"));
        assert!(inject(&cwd_s, "b2", "es").is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn new_brain_update_clears_consumed() {
        let _g = crate::util::test_env_lock();
        let (home, cwd, transcript) = setup("newbrain");
        let cwd_s = cwd.to_string_lossy().to_string();

        assert!(update(&transcript, &cwd_s, "s1", "b1", "Brain One", "es", false).is_some());
        assert!(inject(&cwd_s, "b2", "es").is_some());

        let index_path = home.join("relay").join("index.json");
        let before = read_json(&index_path).expect("index before");
        let key = key_for(&cwd_s);
        assert!(before
            .get(&key)
            .and_then(|v| v.get("consumed"))
            .and_then(|v| v.as_object())
            .map(|o| o.contains_key("b2"))
            .unwrap_or(false));

        assert!(update(&transcript, &cwd_s, "s1", "b3", "Brain Three", "es", true).is_some());

        let after = read_json(&index_path).expect("index after");
        let entry = after.get(&key).expect("entry after");
        assert_eq!(entry.s("brain_id"), "b3");
        assert!(entry
            .get("consumed")
            .and_then(|v| v.as_object())
            .map(|o| o.is_empty())
            .unwrap_or(false));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn read_rejects_bad_keys() {
        let _g = crate::util::test_env_lock();
        let (home, cwd, transcript) = setup("read");
        let cwd_s = cwd.to_string_lossy().to_string();

        assert!(update(&transcript, &cwd_s, "s1", "b1", "Brain One", "es", false).is_some());
        let key = key_for(&cwd_s);
        assert!(read(&key).is_some());
        assert!(read("..").is_none());
        assert!(read("../x").is_none());
        assert!(read("a/b").is_none());
        assert!(read("").is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn compact_caps_and_dedups() {
        let info = json!({
            "cwd": "C:\\work\\proj",
            "task": "x".repeat(500),
            "status": "tests ok",
            "done": ["a", "b", "a", "c", "d", "e", "f"],
            "files": ["C:\\work\\proj\\a.rs", "C:\\work\\proj\\a.rs", "C:\\work\\proj\\b.rs"],
            "commands": ["cargo test", "cargo test", "cargo build", "cargo run"],
            "next": "fix tests"
        });
        let out = compact(
            &info,
            "Brain",
            "proj",
            "es",
            "C:\\relay\\x.full.md",
            "12:00",
        );
        assert!(out.chars().count() <= INJECT_MAX_CHARS);
        assert!(out.contains("a.rs"));
        assert_eq!(out.matches("a.rs").count(), 1);
        assert!(out.contains("cargo test"));
        assert_eq!(out.matches("cargo test").count(), 1);
        let bullets = out.lines().filter(|l| l.starts_with("- ")).count();
        assert!(bullets <= 5);
    }

    #[test]
    fn compact_strips_absolute_paths() {
        let info = json!({
            "cwd": "/home/user/proj",
            "files": ["/home/user/proj/src/main.rs", "/home/user/proj/Cargo.toml"],
            "task": "t",
            "commands": []
        });
        let out = compact(&info, "B", "proj", "en", "/tmp/full.md", "10:00");
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("Cargo.toml"));
        assert!(!out.contains("/home/user/proj"));
    }

    #[test]
    fn compact_caps_with_many_items() {
        let mut files: Vec<String> = Vec::new();
        for i in 0..30 {
            files.push(format!("/very/long/path/to/project/file_{}.rs", i));
        }
        let mut done: Vec<String> = Vec::new();
        for i in 0..30 {
            done.push(format!(
                "did something very long number {} with more text to fill",
                i
            ));
        }
        let info = json!({
            "cwd": "",
            "task": "x".repeat(1000),
            "status": "y".repeat(1000),
            "done": done,
            "files": files,
            "commands": ["cargo test --all-features --verbose", "cargo build --release", "cargo run"],
            "next": "z".repeat(1000)
        });
        let out = compact(
            &info,
            "Brain",
            "proj",
            "es",
            "/very/long/full/path/to/handoff.full.md",
            "12:00",
        );
        assert!(out.chars().count() <= INJECT_MAX_CHARS);
    }
}
