//! MCP servers and skills per brain (Claude Code, OpenCode).
//!
//! Every write to a user configuration file is preceded by a daily backup
//! (`<file>.metabrain-YYYYMMDD.bak`) and keeps every unrelated key intact.

use crate::util::{expand_s, log, now, read_json, write_json, J};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

const MASK: &str = "••••";
const API_REASON: &str = "Este cerebro responde por API y no ejecuta herramientas; instala el MCP en un cerebro Claude Code u OpenCode.";

// ---------------------------------------------------------------------------
// Brain detection
// ---------------------------------------------------------------------------

fn kind_of(brain: &Value) -> String {
    for k in ["kind", "type", "id", "name", "engine", "provider"] {
        if let Some(s) = brain.get(k).and_then(|v| v.as_str()) {
            let l = s.to_lowercase().replace(['-', ' '], "_");
            if l.contains("claude") {
                return "claude_code".to_string();
            }
            if l.contains("opencode") || l.contains("open_code") {
                return "opencode".to_string();
            }
            if l.contains("openai") || l.contains("open_ai") {
                return "openai".to_string();
            }
            if l.contains("anthropic") {
                return "anthropic".to_string();
            }
            if l.contains("ollama") {
                return "ollama".to_string();
            }
        }
    }
    String::new()
}

pub fn supports(brain: &Value) -> (bool, String) {
    match kind_of(brain).as_str() {
        "claude_code" | "opencode" => (true, String::new()),
        "openai" | "anthropic" | "ollama" => (false, API_REASON.to_string()),
        _ => (
            false,
            "Cerebro no compatible con MCP; usa un cerebro Claude Code u OpenCode.".to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn path_key(p: &str) -> String {
    let s = expand_s(p).replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

fn claude_dir(brain: &Value) -> String {
    brain
        .os("config_dir")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "~/.claude".to_string())
}

fn user_mcp_path(brain: &Value) -> String {
    let d = claude_dir(brain);
    if path_key(&d) == path_key("~/.claude") {
        expand_s("~/.claude.json")
    } else {
        PathBuf::from(expand_s(&d))
            .join(".claude.json")
            .to_string_lossy()
            .to_string()
    }
}

/// Validate a user-supplied project directory without expanding environment variables
/// and rejecting UNC / network shares and non-existent paths.
pub fn sanitize_project_dir(project: &str) -> Result<PathBuf, String> {
    let trimmed = project.trim();
    if trimmed.is_empty() {
        return Err("Missing project directory.".into());
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || matches!(c, '%' | '$' | '\0'))
    {
        return Err(
            "Project path cannot contain environment variables or control characters.".into(),
        );
    }
    if trimmed.starts_with("\\\\") || trimmed.starts_with("//") {
        return Err("Network/UNC project paths are not allowed.".into());
    }
    let canon = fs::canonicalize(Path::new(trimmed))
        .map_err(|_| "Project directory does not exist.".to_string())?;
    let s = canon.to_string_lossy();
    if s.starts_with("\\\\?\\UNC\\") || s.starts_with("\\\\.\\") {
        return Err("Network/UNC project paths are not allowed.".into());
    }
    if !canon.is_dir() {
        return Err("Project path is not a directory.".into());
    }
    Ok(canon)
}

fn project_mcp_path(project: &str) -> Result<PathBuf, String> {
    Ok(sanitize_project_dir(project)?.join(".mcp.json"))
}

fn opencode_config_path(brain: &Value) -> PathBuf {
    match brain.os("config").filter(|s| !s.trim().is_empty()) {
        Some(c) => PathBuf::from(expand_s(&c)),
        None => PathBuf::from(expand_s("~/.config/opencode/opencode.json")),
    }
}

fn opencode_base_dir(brain: &Value) -> PathBuf {
    let p = opencode_config_path(brain);
    match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from(expand_s("~/.config/opencode")),
    }
}

fn req_project(project: Option<&str>) -> Result<&str, String> {
    project
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "Falta el proyecto para el ámbito project".to_string())
}

fn skill_dir(
    brain: &Value,
    scope: &str,
    project: Option<&str>,
    name: &str,
) -> Result<PathBuf, String> {
    let kind = kind_of(brain);
    let base = match (kind.as_str(), scope) {
        ("claude_code", "user") => PathBuf::from(expand_s(&claude_dir(brain))).join("skills"),
        ("claude_code", "project") => sanitize_project_dir(req_project(project)?)?
            .join(".claude")
            .join("skills"),
        ("opencode", "user") => opencode_base_dir(brain).join("skills"),
        ("opencode", "project") => sanitize_project_dir(req_project(project)?)?
            .join(".opencode")
            .join("skills"),
        (_, "user") | (_, "project") => {
            return Err("This brain does not support skills.".to_string())
        }
        _ => return Err(format!("Invalid scope: {} (use user or project)", scope)),
    };
    Ok(base.join(name))
}

// ---------------------------------------------------------------------------
// Config-file helpers (read -> modify one section -> write, with backup)
// ---------------------------------------------------------------------------

fn backup_file(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let date = crate::util::local_date(now()).replace('-', "");
    let daily = PathBuf::from(format!("{}.metabrain-{}.bak", path.display(), date));
    if daily.exists() {
        return Ok(());
    }
    fs::copy(path, &daily).map_err(|e| {
        format!(
            "No se pudo crear la copia de seguridad {}: {}",
            daily.display(),
            e
        )
    })?;
    // Keep the legacy rolling name too, but never overwrite an existing one.
    let plain = PathBuf::from(format!("{}.metabrain.bak", path.display()));
    if !plain.exists() {
        let _ = fs::copy(path, &plain);
    }
    Ok(())
}

fn load_obj(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    match read_json(path) {
        Some(Value::Object(m)) => Ok(m),
        Some(_) => Err(format!(
            "El archivo {} no contiene un objeto JSON válido; no se modificó.",
            path.display()
        )),
        None => Err(format!(
            "El archivo {} no es JSON válido; no se modificó.",
            path.display()
        )),
    }
}

fn save_obj(path: &Path, obj: &Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("No se pudo crear {}: {}", parent.display(), e))?;
        }
    }
    backup_file(path)?;
    write_json(path, &Value::Object(obj.clone()))
        .map_err(|e| format!("No se pudo escribir {}: {}", path.display(), e))
}

fn get_section(obj: &Map<String, Value>, key: &str) -> Result<Map<String, Value>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(_) => Err(format!(
            "La sección \"{}\" no es un objeto JSON; no se modificó el archivo.",
            key
        )),
    }
}

fn mcp_location(
    brain: &Value,
    scope: &str,
    project: Option<&str>,
) -> Result<(PathBuf, &'static str), String> {
    match kind_of(brain).as_str() {
        "claude_code" => match scope {
            "user" => Ok((PathBuf::from(user_mcp_path(brain)), "mcpServers")),
            "project" => {
                let p = req_project(project)?;
                Ok((project_mcp_path(p)?, "mcpServers"))
            }
            other => Err(format!("Ámbito no válido: {} (usa user o project)", other)),
        },
        "opencode" => match scope {
            "user" | "project" => Ok((opencode_config_path(brain), "mcp")),
            other => Err(format!("Ámbito no válido: {} (usa user o project)", other)),
        },
        _ => Err("Este cerebro no ejecuta herramientas".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn valid_mcp_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn check_mcp_name(name: &str) -> Result<(), String> {
    if valid_mcp_name(name) {
        Ok(())
    } else {
        Err(format!(
            "Nombre de MCP no válido: \"{}\" (1-64 caracteres [A-Za-z0-9_-])",
            name
        ))
    }
}

fn check_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.chars().count() > 64 {
        return Err(format!(
            "Nombre de skill no válido: \"{}\" (1-64 caracteres [a-z0-9-])",
            name
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "Nombre de skill no válido: \"{}\" (usa minúsculas, números y guiones)",
            name
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry construction / conversion
// ---------------------------------------------------------------------------

fn child_map(e: &Value, keys: &[&str]) -> Map<String, Value> {
    for k in keys {
        if let Some(Value::Object(m)) = e.get(k) {
            return m.clone();
        }
    }
    Map::new()
}

fn command_and_args(e: &Value) -> (String, Vec<Value>) {
    let mut command = String::new();
    let mut args: Vec<Value> = Vec::new();
    if let Some(c) = e.get("command") {
        if let Some(s) = c.as_str() {
            command = s.to_string();
        } else if let Some(a) = c.as_array() {
            let mut it = a.iter();
            if let Some(f) = it.next() {
                command = f.as_str().unwrap_or("").to_string();
            }
            args = it.cloned().collect();
        }
    }
    if let Some(a) = e.get("args").and_then(|v| v.as_array()) {
        args.extend(a.iter().cloned());
    }
    (command, args)
}

fn transport_of(e: &Value) -> &'static str {
    let t = e
        .get("transport")
        .and_then(|v| v.as_str())
        .or_else(|| e.get("type").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_lowercase();
    match t.as_str() {
        "stdio" | "local" => "stdio",
        "sse" => "sse",
        "http" | "remote" | "streamable-http" | "streamable_http" => "http",
        _ => {
            let url = e.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if !url.is_empty() && e.get("command").is_none() {
                "http"
            } else {
                "stdio"
            }
        }
    }
}

fn claude_entry(
    transport: &str,
    command: &str,
    args: &[Value],
    url: &str,
    env: &Map<String, Value>,
    headers: &Map<String, Value>,
) -> Value {
    let mut m = Map::new();
    match transport {
        "stdio" => {
            m.insert("type".to_string(), json!("stdio"));
            m.insert("command".to_string(), json!(command));
            m.insert("args".to_string(), Value::Array(args.to_vec()));
            m.insert("env".to_string(), Value::Object(env.clone()));
        }
        "http" => {
            m.insert("type".to_string(), json!("http"));
            m.insert("url".to_string(), json!(url));
            m.insert("headers".to_string(), Value::Object(headers.clone()));
        }
        _ => {
            m.insert("type".to_string(), json!("sse"));
            m.insert("url".to_string(), json!(url));
            m.insert("headers".to_string(), Value::Object(headers.clone()));
        }
    }
    Value::Object(m)
}

fn opencode_entry(
    transport: &str,
    command: &str,
    args: &[Value],
    url: &str,
    env: &Map<String, Value>,
    headers: &Map<String, Value>,
) -> Value {
    let mut m = Map::new();
    if transport == "stdio" {
        let mut cmd: Vec<Value> = Vec::with_capacity(args.len() + 1);
        cmd.push(Value::String(command.to_string()));
        cmd.extend(args.iter().cloned());
        m.insert("type".to_string(), json!("local"));
        m.insert("command".to_string(), Value::Array(cmd));
        m.insert("environment".to_string(), Value::Object(env.clone()));
        m.insert("enabled".to_string(), json!(true));
    } else {
        m.insert("type".to_string(), json!("remote"));
        m.insert("url".to_string(), json!(url));
        m.insert("headers".to_string(), Value::Object(headers.clone()));
    }
    Value::Object(m)
}

fn mask_map(m: &Map<String, Value>) -> Value {
    let mut o = Map::new();
    for k in m.keys() {
        o.insert(k.clone(), Value::String(MASK.to_string()));
    }
    Value::Object(o)
}

fn entry_to_common(name: &str, scope: &str, e: &Value, source: &str) -> Value {
    let (command, args) = command_and_args(e);
    let transport = transport_of(e);
    let url = e
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let env = child_map(e, &["env", "environment"]);
    let headers = child_map(e, &["headers"]);
    let env_keys: Vec<String> = env.keys().cloned().collect();
    json!({
        "name": name,
        "scope": scope,
        "transport": transport,
        "command": command,
        "args": args,
        "url": url,
        "env_keys": env_keys,
        "env": mask_map(&env),
        "headers": mask_map(&headers),
        "source": source
    })
}

fn entry_to_spec(name: &str, e: &Value) -> Value {
    let (command, args) = command_and_args(e);
    let transport = transport_of(e);
    let url = e
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let env = child_map(e, &["env", "environment"]);
    let headers = child_map(e, &["headers"]);
    json!({
        "name": name,
        "transport": transport,
        "command": command,
        "args": args,
        "url": url,
        "env": Value::Object(env),
        "headers": Value::Object(headers)
    })
}

fn map_of(v: &Value, key: &str) -> Map<String, Value> {
    match v.get(key) {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

fn collect_mcps(path: &Path, section: &str, scope: &str, out: &mut Vec<Value>) {
    if !path.is_file() {
        return;
    }
    let obj = match read_json(path) {
        Some(Value::Object(m)) => m,
        _ => {
            log(&format!(
                "extensions: {} no es JSON válido; se omite",
                path.display()
            ));
            return;
        }
    };
    let sec = match obj.get(section) {
        Some(Value::Object(m)) => m,
        _ => return,
    };
    let source = path.display().to_string();
    for (name, entry) in sec {
        if entry.is_object() {
            out.push(entry_to_common(name, scope, entry, &source));
        }
    }
}

fn collect_skills(dir: &Path, scope: &str, out: &mut Vec<Value>) {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if name.starts_with('.') {
            continue;
        }
        let md = p.join("SKILL.md");
        if !md.is_file() {
            continue;
        }
        let text = fs::read(&md)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default();
        let description = skill_description(&text);
        out.push(json!({
            "name": name,
            "scope": scope,
            "description": description,
            "path": md.display().to_string()
        }));
    }
}

pub fn list(brain: &Value, project: Option<&str>) -> Value {
    let (ok, reason) = supports(brain);
    if !ok {
        return json!({"supported": false, "reason": reason, "mcp": [], "skills": []});
    }

    let kind = kind_of(brain);
    let mut mcp: Vec<Value> = Vec::new();
    let mut skills: Vec<Value> = Vec::new();

    if kind == "claude_code" {
        let up = PathBuf::from(user_mcp_path(brain));
        collect_mcps(&up, "mcpServers", "user", &mut mcp);
        let us = PathBuf::from(expand_s(&claude_dir(brain))).join("skills");
        collect_skills(&us, "user", &mut skills);

        if let Some(p) = project.filter(|s| !s.trim().is_empty()) {
            if let Ok(proj) = sanitize_project_dir(p) {
                let pp = proj.join(".mcp.json");
                collect_mcps(&pp, "mcpServers", "project", &mut mcp);
                let ps = proj.join(".claude").join("skills");
                collect_skills(&ps, "project", &mut skills);
            }
        }
    } else if kind == "opencode" {
        let cp = opencode_config_path(brain);
        collect_mcps(&cp, "mcp", "user", &mut mcp);
        let us = opencode_base_dir(brain).join("skills");
        collect_skills(&us, "user", &mut skills);

        if let Some(p) = project.filter(|s| !s.trim().is_empty()) {
            if let Ok(proj) = sanitize_project_dir(p) {
                let ps = proj.join(".opencode").join("skills");
                collect_skills(&ps, "project", &mut skills);
            }
        }
    }

    json!({"supported": true, "reason": "", "mcp": mcp, "skills": skills})
}

// ---------------------------------------------------------------------------
// MCP mutation
// ---------------------------------------------------------------------------

pub fn add_mcp(
    brain: &Value,
    scope: &str,
    project: Option<&str>,
    spec: &Value,
) -> Result<String, String> {
    let (ok, reason) = supports(brain);
    if !ok {
        return Err(reason);
    }

    let name = spec.s("name").trim().to_string();
    check_mcp_name(&name)?;

    let mut transport = spec.s("transport").trim().to_lowercase();
    if transport.is_empty() {
        transport = "stdio".to_string();
    }
    if transport != "stdio" && transport != "http" && transport != "sse" {
        return Err(format!(
            "Transporte no válido: {} (usa stdio, http o sse)",
            transport
        ));
    }

    let command = spec.s("command").trim().to_string();
    let url = spec.s("url").trim().to_string();

    if transport == "stdio" && command.is_empty() {
        return Err("El transporte stdio requiere \"command\"".to_string());
    }
    if transport != "stdio" && !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!(
            "El transporte {} requiere una \"url\" que empiece por http:// o https://",
            transport
        ));
    }

    let args = spec
        .get("args")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let env = map_of(spec, "env");
    let headers = map_of(spec, "headers");

    let (path, section) = mcp_location(brain, scope, project)?;
    let kind = kind_of(brain);
    let entry = if kind == "opencode" {
        opencode_entry(&transport, &command, &args, &url, &env, &headers)
    } else {
        claude_entry(&transport, &command, &args, &url, &env, &headers)
    };

    let mut obj = load_obj(&path)?;
    let mut sec = get_section(&obj, section)?;
    if sec.contains_key(&name) {
        return Err(format!("Ya existe un MCP llamado {}", name));
    }
    sec.insert(name.clone(), entry);
    obj.insert(section.to_string(), Value::Object(sec));
    save_obj(&path, &obj)?;

    log(&format!(
        "extensions: MCP '{}' añadido en {} ({})",
        name,
        path.display(),
        scope
    ));
    Ok(format!(
        "MCP {} añadido en {} ({})",
        name,
        path.display(),
        scope
    ))
}

pub fn remove_mcp(
    brain: &Value,
    scope: &str,
    project: Option<&str>,
    name: &str,
) -> Result<String, String> {
    let (ok, reason) = supports(brain);
    if !ok {
        return Err(reason);
    }

    let name = name.trim().to_string();
    check_mcp_name(&name)?;

    let (path, section) = mcp_location(brain, scope, project)?;
    let mut obj = load_obj(&path)?;
    let mut sec = get_section(&obj, section)?;
    if sec.remove(&name).is_none() {
        return Err(format!("No existe un MCP llamado {}", name));
    }
    obj.insert(section.to_string(), Value::Object(sec));
    save_obj(&path, &obj)?;

    log(&format!(
        "extensions: MCP '{}' eliminado de {}",
        name,
        path.display()
    ));
    Ok(format!("MCP {} eliminado de {}", name, path.display()))
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn first_nonempty_line(text: &str) -> String {
    for line in text.lines() {
        let l = line.trim();
        if !l.is_empty() && l != "---" {
            return truncate_chars(l, 200);
        }
    }
    String::new()
}

fn skill_description(text: &str) -> String {
    let t = text.trim_start_matches('\u{feff}');
    let mut lines = t.lines().peekable();
    let first = lines.peek().map(|l| l.trim()).unwrap_or("");
    if first == "---" {
        let _ = lines.next();
        let mut desc = String::new();
        let mut closed = false;
        for line in lines.by_ref() {
            let l = line.trim();
            if l == "---" || l == "..." {
                closed = true;
                break;
            }
            if desc.is_empty() {
                if let Some(rest) = l.strip_prefix("description:") {
                    let d = rest
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .trim()
                        .to_string();
                    if !d.is_empty() {
                        desc = truncate_chars(&d, 200);
                    }
                }
            }
        }
        if !desc.is_empty() {
            return desc;
        }
        if closed {
            for line in lines {
                let l = line.trim();
                if !l.is_empty() {
                    return truncate_chars(l, 200);
                }
            }
        }
        return String::new();
    }
    first_nonempty_line(t)
}

pub fn add_skill(
    brain: &Value,
    scope: &str,
    project: Option<&str>,
    name: &str,
    content: &str,
) -> Result<String, String> {
    let (ok, reason) = supports(brain);
    if !ok {
        return Err(reason);
    }

    let name = name.trim();
    check_skill_name(name)?;

    let dir = skill_dir(brain, scope, project, name)?;
    let md = dir.join("SKILL.md");
    if md.exists() {
        return Err(format!("Ya existe una skill llamada {}", name));
    }

    let has_frontmatter = content
        .trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with("---");
    let full = if has_frontmatter {
        content.to_string()
    } else {
        let desc = first_nonempty_line(content);
        format!(
            "---\nname: {}\ndescription: {}\n---\n\n{}",
            name, desc, content
        )
    };

    fs::create_dir_all(&dir).map_err(|e| format!("No se pudo crear {}: {}", dir.display(), e))?;
    fs::write(&md, full).map_err(|e| format!("No se pudo escribir {}: {}", md.display(), e))?;

    log(&format!(
        "extensions: skill '{}' creada en {}",
        name,
        md.display()
    ));
    Ok(format!("Skill {} creada en {}", name, md.display()))
}

pub fn remove_skill(
    brain: &Value,
    scope: &str,
    project: Option<&str>,
    name: &str,
) -> Result<String, String> {
    let (ok, reason) = supports(brain);
    if !ok {
        return Err(reason);
    }

    let name = name.trim();
    check_skill_name(name)?;

    let src = skill_dir(brain, scope, project, name)?;
    if !src.join("SKILL.md").is_file() {
        return Err(format!("No existe la skill {}", name));
    }

    let parent = src
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    // Trash lives next to the "skills" folder (parent of the skill folders).
    let trash = parent
        .parent()
        .map(|p| p.join(".papelera"))
        .unwrap_or_else(|| parent.join(".papelera"));
    fs::create_dir_all(&trash)
        .map_err(|e| format!("No se pudo crear {}: {}", trash.display(), e))?;

    let ts = now() as i64;
    let mut dest = trash.join(format!("{}-{}", name, ts));
    let mut n = 1usize;
    while dest.exists() {
        dest = trash.join(format!("{}-{}-{}", name, ts, n));
        n += 1;
    }

    if fs::rename(&src, &dest).is_err() {
        copy_dir(&src, &dest).map_err(|e| format!("No se pudo mover la skill {}: {}", name, e))?;
        fs::remove_dir_all(&src)
            .map_err(|e| format!("No se pudo borrar la skill original {}: {}", name, e))?;
    }

    log(&format!(
        "extensions: skill '{}' movida a {}",
        name,
        dest.display()
    ));
    Ok(format!("Skill {} movida a {}", name, dest.display()))
}

// ---------------------------------------------------------------------------
// Copy between brains
// ---------------------------------------------------------------------------

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, to)?;
        }
    }
    Ok(())
}

pub fn copy(
    from: &Value,
    to: &Value,
    kind: &str,
    name: &str,
    scope: &str,
    project: Option<&str>,
) -> Result<String, String> {
    let (ok_from, reason_from) = supports(from);
    if !ok_from {
        return Err(reason_from);
    }
    let (ok_to, reason_to) = supports(to);
    if !ok_to {
        return Err(reason_to);
    }

    match kind.trim().to_lowercase().as_str() {
        "mcp" => {
            let name = name.trim();
            check_mcp_name(name)?;

            let (path, section) = mcp_location(from, scope, project)?;
            let obj = load_obj(&path)?;
            let sec = get_section(&obj, section)?;
            let entry = sec
                .get(name)
                .cloned()
                .ok_or_else(|| format!("No existe un MCP llamado {}", name))?;

            let spec = entry_to_spec(name, &entry);
            add_mcp(to, scope, project, &spec)
        }
        "skill" => {
            let name = name.trim();
            check_skill_name(name)?;

            let src = skill_dir(from, scope, project, name)?;
            if !src.join("SKILL.md").is_file() {
                return Err(format!(
                    "No existe la skill {} en el cerebro de origen",
                    name
                ));
            }
            let dst = skill_dir(to, scope, project, name)?;
            if dst.exists() {
                return Err(format!(
                    "Ya existe una skill llamada {} en el cerebro de destino",
                    name
                ));
            }
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("No se pudo crear {}: {}", parent.display(), e))?;
            }
            copy_dir(&src, &dst)
                .map_err(|e| format!("No se pudo copiar la skill {}: {}", name, e))?;

            log(&format!(
                "extensions: skill '{}' copiada de {} a {}",
                name,
                src.display(),
                dst.display()
            ));
            Ok(format!("Skill {} copiada a {}", name, dst.display()))
        }
        _ => Err(format!("Tipo no válido: {} (usa mcp o skill)", kind)),
    }
}

// ---------------------------------------------------------------------------
// Parsing pasted MCP JSON
// ---------------------------------------------------------------------------

fn looks_like_single(o: &Map<String, Value>) -> bool {
    matches!(
        o.get("command"),
        Some(Value::String(_)) | Some(Value::Array(_))
    ) || matches!(o.get("url"), Some(Value::String(_)))
}

fn normalize_spec(name: &str, raw: &Value) -> Value {
    let t = raw
        .get("transport")
        .and_then(|v| v.as_str())
        .or_else(|| raw.get("type").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_lowercase();

    let (command, mut args) = command_and_args(raw);
    let url = raw
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let transport = match t.as_str() {
        "local" | "stdio" => "stdio",
        "sse" => "sse",
        "remote" | "http" | "streamable-http" | "streamable_http" => {
            if url.is_empty() {
                "stdio"
            } else {
                "http"
            }
        }
        _ => {
            if !url.is_empty() && command.is_empty() {
                "http"
            } else {
                "stdio"
            }
        }
    };

    if transport == "stdio" && command.is_empty() {
        // Leave it to add_mcp to report the precise validation error.
        args.clear();
    }

    let env = raw
        .get("env")
        .or_else(|| raw.get("environment"))
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let headers = raw
        .get("headers")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));

    json!({
        "name": name,
        "transport": transport,
        "command": command,
        "args": args,
        "url": url,
        "env": env,
        "headers": headers
    })
}

pub fn parse_mcp_json(text: &str) -> Result<Vec<Value>, String> {
    let txt = text.trim().trim_start_matches('\u{feff}');
    if txt.is_empty() {
        return Err("No se recibió JSON de MCP".to_string());
    }
    let v: Value = serde_json::from_str(txt).map_err(|e| format!("JSON no válido: {}", e))?;
    let obj = match v {
        Value::Object(m) => m,
        _ => return Err("Se esperaba un objeto JSON con MCPs".to_string()),
    };

    if let Some(servers) = obj.get("mcpServers") {
        let map = match servers {
            Value::Object(m) => m,
            _ => return Err("\"mcpServers\" debe ser un objeto".to_string()),
        };
        let mut out = Vec::new();
        for (name, entry) in map {
            if !entry.is_object() {
                return Err(format!("La entrada MCP \"{}\" no es un objeto", name));
            }
            out.push(normalize_spec(name, entry));
        }
        return Ok(out);
    }

    if looks_like_single(&obj) {
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| valid_mcp_name(s))
            .unwrap_or("nuevo-mcp")
            .to_string();
        return Ok(vec![normalize_spec(&name, &Value::Object(obj))]);
    }

    let mut out = Vec::new();
    for (name, entry) in &obj {
        if !entry.is_object() {
            return Err(format!("La entrada \"{}\" no es un objeto MCP", name));
        }
        out.push(normalize_spec(name, entry));
    }
    if out.is_empty() {
        return Err("No se encontró ningún MCP".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("mb_ext_{}_{}_{}", tag, std::process::id(), n));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("tmp dir");
        p
    }

    fn claude_brain(dir: &Path) -> Value {
        json!({"kind": "claude_code", "config_dir": dir.to_string_lossy().to_string()})
    }

    #[test]
    fn user_mcp_roundtrip_preserves_other_keys() {
        let base = tmp("user");
        let cfg = base.join(".claude.json");
        fs::write(
            &cfg,
            r#"{"theme":"dark","mcpServers":{"old":{"type":"stdio","command":"old"}},"other":42}"#,
        )
        .unwrap();
        let brain = claude_brain(&base);

        let spec = json!({
            "name": "srv_1",
            "transport": "stdio",
            "command": "npx",
            "args": ["-y", "pkg"],
            "env": {"TOKEN": "secret"}
        });
        add_mcp(&brain, "user", None, &spec).expect("add user mcp");

        let v = crate::util::read_json(&cfg).expect("json");
        assert_eq!(v.get("theme").and_then(|x| x.as_str()), Some("dark"));
        assert_eq!(v.get("other").and_then(|x| x.as_i64()), Some(42));
        assert_eq!(
            v.get("mcpServers")
                .and_then(|x| x.get("old"))
                .and_then(|x| x.get("command"))
                .and_then(|x| x.as_str()),
            Some("old")
        );
        assert_eq!(
            v.get("mcpServers")
                .and_then(|x| x.get("srv_1"))
                .and_then(|x| x.get("type"))
                .and_then(|x| x.as_str()),
            Some("stdio")
        );

        let l = list(&brain, None);
        assert_eq!(l.get("supported").and_then(|x| x.as_bool()), Some(true));
        let mcps = l
            .get("mcp")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(mcps.iter().any(|m| {
            m.get("name").and_then(|x| x.as_str()) == Some("srv_1")
                && m.get("transport").and_then(|x| x.as_str()) == Some("stdio")
        }));
        let rendered = l.to_string();
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains(MASK));

        let date = crate::util::local_date(crate::util::now()).replace('-', "");
        assert!(base
            .join(format!(".claude.json.metabrain-{}.bak", date))
            .is_file());

        remove_mcp(&brain, "user", None, "srv_1").expect("remove");
        let v2 = crate::util::read_json(&cfg).expect("json2");
        assert!(v2.get("mcpServers").and_then(|x| x.get("srv_1")).is_none());
        assert_eq!(v2.get("theme").and_then(|x| x.as_str()), Some("dark"));
    }

    #[test]
    fn project_mcp_lives_in_project() {
        let base = tmp("projbase");
        let proj = tmp("projdir");
        let brain = claude_brain(&base);
        let p = proj.to_string_lossy().to_string();

        add_mcp(
            &brain,
            "project",
            Some(&p),
            &json!({
                "name": "p1",
                "transport": "http",
                "url": "https://example.test/mcp",
                "headers": {"X-Token": "mb-test-header-value-must-be-masked"}
            }),
        )
        .expect("add project");

        let file = proj.join(".mcp.json");
        assert!(file.is_file());
        let v = crate::util::read_json(&file).expect("json");
        assert_eq!(
            v.get("mcpServers")
                .and_then(|x| x.get("p1"))
                .and_then(|x| x.get("type"))
                .and_then(|x| x.as_str()),
            Some("http")
        );

        let l = list(&brain, Some(&p));
        let mcps = l
            .get("mcp")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(mcps
            .iter()
            .any(|m| m.get("scope").and_then(|x| x.as_str()) == Some("project")));
        assert!(!l
            .to_string()
            .contains("mb-test-header-value-must-be-masked"));
    }

    #[test]
    fn invalid_json_is_untouched() {
        let base = tmp("bad");
        let cfg = base.join(".claude.json");
        fs::write(&cfg, "{not json").unwrap();
        let brain = claude_brain(&base);

        let r = add_mcp(
            &brain,
            "user",
            None,
            &json!({"name": "x", "transport": "stdio", "command": "c"}),
        );
        assert!(r.is_err());
        assert_eq!(fs::read_to_string(&cfg).unwrap(), "{not json");
    }

    #[test]
    fn opencode_converts_format() {
        let base = tmp("oc");
        let cfg = base.join("opencode.json");
        let brain = json!({"kind": "opencode", "config": cfg.to_string_lossy().to_string()});

        add_mcp(
            &brain,
            "user",
            None,
            &json!({
                "name": "loc",
                "transport": "stdio",
                "command": "npx",
                "args": ["-y", "x"],
                "env": {"K": "v"}
            }),
        )
        .expect("add local");
        add_mcp(
            &brain,
            "user",
            None,
            &json!({
                "name": "rem",
                "transport": "http",
                "url": "https://example.test/mcp",
                "headers": {"Authorization": "Bearer z"}
            }),
        )
        .expect("add remote");

        let v = crate::util::read_json(&cfg).expect("json");
        let loc = v.get("mcp").and_then(|x| x.get("loc")).expect("loc entry");
        assert_eq!(loc.get("type").and_then(|x| x.as_str()), Some("local"));
        assert_eq!(
            loc.get("command")
                .and_then(|x| x.as_array())
                .map(|a| a.len()),
            Some(3)
        );
        assert_eq!(
            loc.get("environment")
                .and_then(|x| x.get("K"))
                .and_then(|x| x.as_str()),
            Some("v")
        );
        assert_eq!(loc.get("enabled").and_then(|x| x.as_bool()), Some(true));

        let rem = v.get("mcp").and_then(|x| x.get("rem")).expect("rem entry");
        assert_eq!(rem.get("type").and_then(|x| x.as_str()), Some("remote"));
        assert_eq!(
            rem.get("headers")
                .and_then(|x| x.get("Authorization"))
                .and_then(|x| x.as_str()),
            Some("Bearer z")
        );

        let rendered = list(&brain, None).to_string();
        assert!(!rendered.contains("Bearer z"));
        assert!(rendered.contains(MASK));
    }

    #[test]
    fn skills_add_with_and_without_frontmatter() {
        let base = tmp("skills");
        let brain = claude_brain(&base);

        add_skill(&brain, "user", None, "demo", "# Demo skill\nDoes things")
            .expect("add no frontmatter");
        let md = base.join("skills").join("demo").join("SKILL.md");
        let txt = fs::read_to_string(&md).expect("read");
        assert!(txt.starts_with("---\nname: demo\ndescription: # Demo skill\n---\n\n"));
        assert!(txt.ends_with("# Demo skill\nDoes things"));

        add_skill(
            &brain,
            "user",
            None,
            "other",
            "---\nname: other\ndescription: custom\n---\n\nBody",
        )
        .expect("add frontmatter");
        let md2 = base.join("skills").join("other").join("SKILL.md");
        assert_eq!(
            fs::read_to_string(&md2).unwrap(),
            "---\nname: other\ndescription: custom\n---\n\nBody"
        );

        let l = list(&brain, None);
        let skills = l
            .get("skills")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(skills.iter().any(|s| {
            s.get("name").and_then(|x| x.as_str()) == Some("other")
                && s.get("description").and_then(|x| x.as_str()) == Some("custom")
        }));

        assert!(add_skill(&brain, "user", None, "demo", "x").is_err());
        assert!(add_skill(&brain, "user", None, "Bad_Name", "x").is_err());
    }

    #[test]
    fn remove_skill_moves_to_trash() {
        let base = tmp("trash");
        let brain = claude_brain(&base);
        add_skill(&brain, "user", None, "gone", "body").expect("add");
        remove_skill(&brain, "user", None, "gone").expect("remove");

        assert!(!base.join("skills").join("gone").exists());
        let trash = base.join(".papelera");
        assert!(trash.is_dir());
        let found = fs::read_dir(&trash)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("gone-"));
        assert!(found);
    }

    #[test]
    fn copy_skill_between_brains() {
        let a = tmp("copya");
        let b = tmp("copyb");
        let ba = claude_brain(&a);
        let bb = claude_brain(&b);

        add_skill(&ba, "user", None, "s1", "hello").expect("add");
        copy(&ba, &bb, "skill", "s1", "user", None).expect("copy");
        assert!(b.join("skills").join("s1").join("SKILL.md").is_file());
        assert!(copy(&ba, &bb, "skill", "s1", "user", None).is_err());
    }

    #[test]
    fn parse_three_formats() {
        let a = parse_mcp_json(r#"{"mcpServers":{"one":{"command":"npx","args":["-y","p"]}}}"#)
            .expect("mcpServers format");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].get("name").and_then(|x| x.as_str()), Some("one"));
        assert_eq!(
            a[0].get("transport").and_then(|x| x.as_str()),
            Some("stdio")
        );
        assert_eq!(a[0].get("command").and_then(|x| x.as_str()), Some("npx"));

        let b =
            parse_mcp_json(r#"{"two":{"command":"node","args":["s.js"]}}"#).expect("map format");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].get("name").and_then(|x| x.as_str()), Some("two"));
        assert_eq!(b[0].get("command").and_then(|x| x.as_str()), Some("node"));

        let c = parse_mcp_json(r#"{"command":"uvx","args":["foo"]}"#).expect("single format");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].get("name").and_then(|x| x.as_str()), Some("nuevo-mcp"));
        assert_eq!(c[0].get("command").and_then(|x| x.as_str()), Some("uvx"));
    }

    #[test]
    fn invalid_names_and_unsupported_brains() {
        let base = tmp("names");
        let brain = claude_brain(&base);
        assert!(add_mcp(
            &brain,
            "user",
            None,
            &json!({"name": "bad name", "transport": "stdio", "command": "x"})
        )
        .is_err());
        assert!(add_mcp(
            &brain,
            "user",
            None,
            &json!({"name": "ok", "transport": "stdio"})
        )
        .is_err());
        assert!(add_skill(&brain, "user", None, "UPPER", "x").is_err());
        assert!(!supports(&json!({"kind": "openai"})).0);
        assert!(!supports(&json!({"kind": "anthropic"})).0);
        assert!(!supports(&json!({"kind": "ollama"})).0);
    }
}
