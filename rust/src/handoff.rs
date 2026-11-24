//! Handoff: rebuild a session brief from the session log WITHOUT spending that account's quota.
//! Port of metabrain/handoff.py.
use crate::util::{data_dir, http_json, log, secret, truthy, J};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
const DEFAULT_MAX_LINES: usize = 200_000;

// ---------------------------------------------------------------- helpers

/// Python `_text`: a plain string, or the text blocks of a message content list.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .map(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect::<Vec<String>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Python `_clip`: trim, then cut to n chars adding an ellipsis.
fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn arr_push(root: &mut Value, key: &str, item: Value) {
    if let Some(a) = root.get_mut(key).and_then(|v| v.as_array_mut()) {
        a.push(item);
    }
}

fn str_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- collect

pub fn collect(path: &Path) -> Value {
    collect_with(path, DEFAULT_MAX_LINES)
}

/// Rebuild the picture of the session from its JSONL log.
pub fn collect_with(path: &Path, max_lines: usize) -> Value {
    let mut info = json!({
        "cwd": "", "task": "", "users": [], "assistant": [], "files": [], "commands": [], "todos": {},
        "tasks": [], "errors": [], "model": "", "last_ts": ""
    });
    let Ok(f) = fs::File::open(path) else {
        return info;
    };
    let mut reader = BufReader::new(f);
    let mut buf: Vec<u8> = Vec::new();
    let mut i: usize = 0;
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        if i > max_lines {
            break;
        }
        // Python opened the file with errors="replace".
        let raw = String::from_utf8_lossy(&buf);
        let raw = raw.trim_end_matches(['\n', '\r']);
        if raw.starts_with("{\"type\":\"api-request") || raw.starts_with("{\"type\": \"api-request")
        {
            continue;
        }
        i += 1;
        let Ok(d) = serde_json::from_str::<Value>(raw) else {
            continue;
        };

        if info.s("cwd").is_empty() {
            if let Some(c) = d.get("cwd").and_then(|v| v.as_str()) {
                if !c.is_empty() {
                    info["cwd"] = Value::String(c.to_string());
                }
            }
        }
        if let Some(ts) = d.get("timestamp").and_then(|v| v.as_str()) {
            if !ts.is_empty() {
                info["last_ts"] = Value::String(ts.to_string());
            }
        }

        let t = d
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if t == "queue-operation" && info.s("task").is_empty() {
            if let Some(c) = d.get("content").and_then(|v| v.as_str()) {
                if !c.is_empty() {
                    info["task"] = Value::String(c.to_string());
                }
            }
        }

        if t == "user" && !truthy(d.get("isMeta")) {
            let c = d.get("message").and_then(|m| m.get("content")).cloned();
            let txt = text_of(c.as_ref().unwrap_or(&Value::Null))
                .trim()
                .to_string();
            if !txt.is_empty() && !txt.starts_with('<') {
                if info.s("task").is_empty() {
                    info["task"] = Value::String(txt.clone());
                }
                arr_push(&mut info, "users", Value::String(txt));
            }
            if let Some(Value::Array(blocks)) = &c {
                for b in blocks {
                    if b.get("type").and_then(|v| v.as_str()) == Some("tool_result")
                        && truthy(b.get("is_error"))
                    {
                        let msg: String = text_of(b.get("content").unwrap_or(&Value::Null))
                            .chars()
                            .take(300)
                            .collect();
                        arr_push(&mut info, "errors", Value::String(msg));
                    }
                }
            }
        }

        if t == "assistant" {
            let m = d.get("message").cloned().unwrap_or_else(|| json!({}));
            if let Some(model) = m.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    info["model"] = Value::String(model.to_string());
                }
            }
            let blocks = m
                .get("content")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for b in &blocks {
                match b.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "text" => {
                        let tx = b
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if !tx.is_empty() {
                            arr_push(&mut info, "assistant", Value::String(tx));
                        }
                    }
                    "tool_use" => {
                        let n = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let inp = b.get("input").cloned().unwrap_or_else(|| json!({}));
                        if EDIT_TOOLS.contains(&n) {
                            let p = inp
                                .get("file_path")
                                .and_then(|v| v.as_str())
                                .or_else(|| inp.get("notebook_path").and_then(|v| v.as_str()));
                            if let Some(p) = p.filter(|s| !s.is_empty()) {
                                if let Some(a) =
                                    info.get_mut("files").and_then(|v| v.as_array_mut())
                                {
                                    a.retain(|x| x.as_str() != Some(p));
                                    a.push(Value::String(p.to_string()));
                                }
                            }
                        } else if n == "Bash" || n == "PowerShell" {
                            if let Some(cmd) = inp
                                .get("command")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                            {
                                let c: String = cmd.chars().take(200).collect();
                                arr_push(&mut info, "commands", Value::String(c));
                            }
                        } else if n == "TodoWrite" {
                            let mut todos = Map::new();
                            if let Some(list) = inp.get("todos").and_then(|v| v.as_array()) {
                                for x in list {
                                    let k = x.get("content").and_then(|v| v.as_str()).unwrap_or("");
                                    let s = x.get("status").and_then(|v| v.as_str()).unwrap_or("");
                                    todos.insert(k.to_string(), Value::String(s.to_string()));
                                }
                            }
                            info["todos"] = Value::Object(todos);
                        } else if n == "TaskCreate" {
                            if let Some(subj) = inp
                                .get("subject")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                            {
                                arr_push(&mut info, "tasks", Value::String(subj.to_string()));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Populate canonical summary fields (`status`, `done`, `next`) so `relay::compact`
    // and downstream consumers share one contract with `handoff::collect_with`.
    let assistant_msgs = str_list(&info, "assistant");
    let user_msgs = str_list(&info, "users");
    let errors = str_list(&info, "errors");
    let mut done_items: Vec<String> = Vec::new();
    if let Some(todos) = info.get("todos").and_then(|v| v.as_object()) {
        for (content, st) in todos {
            if st.as_str() == Some("completed") {
                done_items.push(clip(content, 120));
            }
        }
    }
    if done_items.is_empty() {
        for msg in assistant_msgs.iter().rev().take(5).rev() {
            let line = clip(msg, 120);
            if !line.is_empty() {
                done_items.push(line);
            }
        }
    }
    let status_text = if let Some(last_err) = errors.last() {
        format!("Last error: {}", clip(last_err, 160))
    } else if let Some(last_asst) = assistant_msgs.last() {
        clip(last_asst, 200)
    } else {
        String::new()
    };
    let mut next_text = String::new();
    if let Some(todos) = info.get("todos").and_then(|v| v.as_object()) {
        if let Some((pending, _)) = todos
            .iter()
            .find(|(_, st)| st.as_str() != Some("completed"))
        {
            next_text = clip(pending, 200);
        }
    }
    if next_text.is_empty() {
        if let Some(last_u) = user_msgs.last() {
            next_text = clip(last_u, 200);
        }
    }
    info["status"] = Value::String(status_text);
    info["done"] = Value::Array(done_items.into_iter().map(Value::String).collect());
    info["next"] = Value::String(next_text);
    info
}

// ---------------------------------------------------------------- render

pub fn render(info: &Value, brain_name: &str, account: &str, reason: &str, lang: &str) -> String {
    let es = lang == "es";
    let mut l: Vec<String> = Vec::new();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    l.push(format!("# HANDOFF · {brain_name} · {now}"));
    l.push(String::new());
    let head = if es {
        "> Escrito por MetaBrain desde el registro de la sesión (sin gastar cuota). Motivo: "
    } else {
        "> Written by MetaBrain from the session log (no quota spent). Reason: "
    };
    l.push(format!("{head}{reason}"));
    l.push(String::new());
    l.push(format!(
        "## {}",
        if es {
            "Tarea original"
        } else {
            "Original task"
        }
    ));
    let task = clip(&info.s("task"), 1500);
    l.push(if task.is_empty() {
        "—".to_string()
    } else {
        task
    });

    let users = str_list(info, "users");
    if users.len() > 1 {
        l.push(String::new());
        l.push(format!(
            "## {}",
            if es {
                "Últimos pedidos del usuario"
            } else {
                "Latest user requests"
            }
        ));
        let start = users.len().saturating_sub(6);
        for u in &users[start..] {
            l.push(format!("- {}", clip(u, 400).replace('\n', " ")));
        }
    }

    let assistant = str_list(info, "assistant");
    if !assistant.is_empty() {
        l.push(String::new());
        l.push(format!(
            "## {}",
            if es {
                "Lo último que dijo el asistente"
            } else {
                "What the assistant said last"
            }
        ));
        let start = assistant.len().saturating_sub(3);
        for a in &assistant[start..] {
            l.push(clip(a, 900));
            l.push(String::new());
        }
    }

    let todos: Map<String, Value> = info
        .get("todos")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let tasks = str_list(info, "tasks");
    if !todos.is_empty() || !tasks.is_empty() {
        l.push(format!("## {}", if es { "Pendientes" } else { "To-dos" }));
        for (k, v) in &todos {
            let done = v.as_str() == Some("completed");
            l.push(format!("- [{}] {}", if done { 'x' } else { ' ' }, k));
        }
        let start = tasks.len().saturating_sub(12);
        for t in &tasks[start..] {
            l.push(format!("- {t}"));
        }
        l.push(String::new());
    }

    let files = str_list(info, "files");
    if !files.is_empty() {
        l.push(format!(
            "## {}",
            if es {
                "Archivos tocados (más reciente al final)"
            } else {
                "Files touched (latest last)"
            }
        ));
        let start = files.len().saturating_sub(30);
        for p in &files[start..] {
            l.push(format!("- `{p}`"));
        }
        l.push(String::new());
    }

    let commands = str_list(info, "commands");
    if !commands.is_empty() {
        l.push(format!(
            "## {}",
            if es {
                "Últimos comandos"
            } else {
                "Last commands"
            }
        ));
        let start = commands.len().saturating_sub(8);
        for c in &commands[start..] {
            l.push(format!("- `{c}`"));
        }
        l.push(String::new());
    }

    let errors = str_list(info, "errors");
    if !errors.is_empty() {
        l.push(format!(
            "## {}",
            if es {
                "Errores recientes"
            } else {
                "Recent errors"
            }
        ));
        let start = errors.len().saturating_sub(5);
        for e in &errors[start..] {
            l.push(format!("- {}", clip(e, 250).replace('\n', " ")));
        }
        l.push(String::new());
    }

    l.push(format!(
        "## {}",
        if es {
            "Cómo seguir"
        } else {
            "How to continue"
        }
    ));
    l.push(if es {
        "Lee este archivo, revisa los archivos tocados y continúa desde el último pedido. \
         No repitas trabajo ya hecho; si algo no está claro, verifica en los archivos antes de cambiar nada."
            .to_string()
    } else {
        "Read this file, check the files touched and continue from the latest request. \
         Do not redo finished work; when unsure, verify in the files before changing anything."
            .to_string()
    });
    l.push(format!(
        "\n<!-- cwd: {} · {} · model: {} -->",
        info.s("cwd"),
        account,
        info.s("model")
    ));
    l.join("\n") + "\n"
}

// ---------------------------------------------------------------- summarize

/// Compress with a cheap OpenAI-compatible brain. Returns md unchanged if anything fails.
pub fn summarize(md: &str, brain_cfg: &Value, lang: &str) -> String {
    let key_env = brain_cfg
        .get("key_env")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let Some(key) = secret(key_env) else {
        return md.to_string();
    };

    let ladder = brain_cfg
        .get("ladder")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let model = match ladder.last() {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "deepseek-flash".to_string(),
    };

    let sys_msg = if lang == "es" {
        "Convierte este registro en un HANDOFF claro y corto en markdown para que otro asistente continúe el trabajo: \
         objetivo, estado actual, decisiones tomadas, archivos clave, siguiente paso exacto, riesgos. No inventes nada."
    } else {
        "Turn this log into a short, clear markdown HANDOFF so another assistant can continue: goal, current state, \
         decisions, key files, exact next step, risks. Do not invent anything."
    };

    let mut body = json!({
        "model": model,
        "max_tokens": 1500,
        "messages": [
            {"role": "system", "content": sys_msg},
            {"role": "user", "content": md.chars().take(60000).collect::<String>()}
        ]
    });

    let base = brain_cfg.s("base_url");
    if base.is_empty() {
        log("handoff summarize failed: missing base_url");
        return md.to_string();
    }
    if base.contains("deepseek") {
        body["thinking"] = json!({"type": "disabled"});
    }

    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let auth = format!("Bearer {key}");
    let headers: [(&str, &str); 2] = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    let res: Result<String, String> = http_json(&url, &headers, Some(&body), 90).and_then(|r| {
        r.get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .ok_or_else(|| "invalid response".to_string())
    });

    match res {
        Ok(out) => format!(
            "{out}\n\n---\n<details><summary>Registro completo</summary>\n\n{md}\n</details>\n"
        ),
        Err(e) => {
            log(&format!("handoff summarize failed: {e}"));
            md.to_string()
        }
    }
}

// ---------------------------------------------------------------- write_handoff

pub fn write_handoff(
    session_path: &Path,
    brain_name: &str,
    account: &str,
    reason: &str,
    lang: &str,
    summarizer: Option<&Value>,
) -> (Vec<PathBuf>, Value) {
    let info = collect(session_path);
    let mut md = render(&info, brain_name, account, reason, lang);
    if let Some(cfg) = summarizer {
        md = summarize(&md, cfg, lang);
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let folder = data_dir().join("handoffs");
    let _ = fs::create_dir_all(&folder);
    static UNSAFE_FILENAME_CHARS: OnceLock<regex::Regex> = OnceLock::new();
    let safe = UNSAFE_FILENAME_CHARS
        .get_or_init(|| regex::Regex::new(r"[^A-Za-z0-9]+").unwrap())
        .replace_all(brain_name, "_");
    let name = format!("{}_{}.md", chrono::Local::now().format("%Y%m%d-%H%M"), safe);
    let p = folder.join(name);
    if fs::write(&p, md.as_bytes()).is_ok() {
        out.push(p);
    }
    let cwd = info.s("cwd");
    if !cwd.is_empty() && Path::new(&cwd).is_dir() {
        let q = Path::new(&cwd).join("HANDOFF_METABRAIN.md");
        if fs::write(&q, md.as_bytes()).is_ok() {
            out.push(q);
        }
    }
    (out, info)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mb_handoff_{}_{}", tag, std::process::id()));
        let _ = fs::create_dir_all(&d);
        d
    }

    #[test]
    fn text_of_string_and_blocks() {
        assert_eq!(text_of(&json!("hi")), "hi");
        let blocks = json!([
            {"type": "text", "text": "a"},
            {"type": "tool_use", "name": "Edit"},
            {"type": "text", "text": "b"}
        ]);
        assert_eq!(text_of(&blocks), "a\nb");
        assert_eq!(text_of(&Value::Null), "");
        assert_eq!(text_of(&json!(5)), "");
    }

    #[test]
    fn clip_trims_and_marks_truncation() {
        assert_eq!(clip("  hi  ", 10), "hi");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("", 5), "");
        assert_eq!(clip("abcd", 4), "abcd");
    }

    #[test]
    fn collect_reads_a_session() {
        let dir = tmp_dir("collect");
        let p = dir.join("s.jsonl");
        let lines = [json!({"type":"user","cwd":"C:/work","timestamp":"2024-01-01T10:00:00Z",
                   "message":{"content":"Build the thing"}})
            .to_string(),
            json!({"type":"queue-operation","content":"queued task"}).to_string(),
            json!({"type":"assistant","message":{"model":"claude-x","content":[
                {"type":"text","text":"ok"},
                {"type":"tool_use","name":"Edit","input":{"file_path":"a.rs"}},
                {"type":"tool_use","name":"Bash","input":{"command":"cargo build"}}]}})
            .to_string(),
            json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"ship","status":"completed"}]}},
                {"type":"tool_use","name":"TaskCreate","input":{"subject":"review"}}]}})
            .to_string(),
            json!({"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"boom"}]}})
                .to_string(),
            json!({"type":"api-request","x":1}).to_string(),
            "not json".to_string()];
        fs::write(&p, lines.join("\n") + "\n").unwrap();

        let info = collect(&p);
        assert_eq!(info.s("task"), "Build the thing");
        assert_eq!(info.s("cwd"), "C:/work");
        assert_eq!(info.s("model"), "claude-x");
        assert_eq!(info.s("last_ts"), "2024-01-01T10:00:00Z");
        assert_eq!(
            str_list(&info, "users"),
            vec!["Build the thing".to_string()]
        );
        assert_eq!(str_list(&info, "files"), vec!["a.rs".to_string()]);
        assert_eq!(str_list(&info, "commands"), vec!["cargo build".to_string()]);
        assert_eq!(str_list(&info, "tasks"), vec!["review".to_string()]);
        assert_eq!(str_list(&info, "errors"), vec!["boom".to_string()]);
        assert_eq!(
            info.get("todos")
                .and_then(|v| v.get("ship"))
                .and_then(|v| v.as_str()),
            Some("completed")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_missing_file_is_empty() {
        let info = collect(Path::new("this-file-does-not-exist-42.jsonl"));
        assert_eq!(info.s("task"), "");
        assert!(str_list(&info, "users").is_empty());
        assert!(info
            .get("todos")
            .and_then(|v| v.as_object())
            .map(|o| o.is_empty())
            .unwrap_or(false));
    }

    #[test]
    fn render_spanish_and_english() {
        let info = json!({"cwd":"C:/w","task":"do it","users":["do it","next"],"assistant":["done"],
            "files":["x.rs"],"commands":["ls"],"todos":{"a":"pending","b":"completed"},"tasks":[],
            "errors":[],"model":"m","last_ts":""});
        let md = render(&info, "brain", "acct", "compact", "es");
        assert!(md.starts_with("# HANDOFF · brain · "));
        assert!(md.contains("> Escrito por MetaBrain desde el registro de la sesión (sin gastar cuota). Motivo: compact"));
        assert!(md.contains("## Tarea original\ndo it"));
        assert!(md.contains("## Últimos pedidos del usuario\n- do it\n- next"));
        assert!(md.contains("## Pendientes\n- [ ] a\n- [x] b\n"));
        assert!(md.contains("## Archivos tocados (más reciente al final)\n- `x.rs`\n"));
        assert!(md.contains("## Cómo seguir\n"));
        assert!(md.ends_with("<!-- cwd: C:/w · acct · model: m -->\n"));

        let en = render(&json!({"task": ""}), "b", "a", "r", "en");
        assert!(
            en.contains("> Written by MetaBrain from the session log (no quota spent). Reason: r")
        );
        assert!(en.contains("## Original task\n—"));
        assert!(en.contains("## How to continue\n"));
    }

    #[test]
    fn summarize_without_key_returns_input() {
        std::env::remove_var("METABRAIN_TEST_KEY_DOES_NOT_EXIST_42");
        let cfg = json!({"key_env":"METABRAIN_TEST_KEY_DOES_NOT_EXIST_42",
                         "base_url":"https://api.deepseek.com","ladder":["x","y"]});
        assert_eq!(summarize("# md", &cfg, "es"), "# md");
        assert_eq!(summarize("# md", &json!({}), "en"), "# md");
    }

    #[test]
    fn write_handoff_writes_both_files() {
        let _g = crate::util::test_env_lock();
        let dir = tmp_dir("write");
        std::env::set_var("METABRAIN_HOME", &dir);
        let cwd = dir.join("proj");
        let _ = fs::create_dir_all(&cwd);
        let sess = dir.join("s.jsonl");
        let line = json!({"type":"user","cwd": cwd.to_string_lossy(),
                          "message":{"content":"hello there"}})
        .to_string();
        fs::write(&sess, line + "\n").unwrap();

        let (out, info) = write_handoff(&sess, "my brain", "acc", "test", "en", None);
        assert_eq!(info.s("cwd"), cwd.to_string_lossy().to_string());
        assert_eq!(out.len(), 2);
        let md = fs::read_to_string(&out[0]).unwrap_or_default();
        assert!(md.contains("# HANDOFF · my brain · "));
        let name = out[0]
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        assert!(name.contains("my_brain"));
        assert!(cwd.join("HANDOFF_METABRAIN.md").is_file());
        let copy = fs::read_to_string(cwd.join("HANDOFF_METABRAIN.md")).unwrap_or_default();
        assert_eq!(copy, md);
        let _ = fs::remove_dir_all(&dir);
    }
}
