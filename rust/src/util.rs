//! Small shared helpers: paths, JSON files, time, log, secrets, HTTP.
use serde_json::{json, Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const NO_WINDOW: u32 = 0x0800_0000;
pub const DETACHED: u32 = 0x0000_0008;

static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Seconds since the epoch, as f64 (same unit the Python version used everywhere).
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn home() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where config, state and ledgers live. Portable mode: a "data" folder next to the exe.
pub fn data_dir() -> PathBuf {
    let d = if let Some(h) = std::env::var_os("METABRAIN_HOME") {
        PathBuf::from(h)
    } else if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("data")))
        .filter(|p| p.is_dir())
    {
        p
    } else if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("MetaBrain")
    } else {
        home().join(".metabrain")
    };
    let _ = fs::create_dir_all(&d);
    d
}

pub fn log(msg: &str) {
    let p = data_dir().join("metabrain.log");
    let line = format!(
        "{} {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        msg
    );
    let _g = LOG_LOCK.lock();
    if fs::metadata(&p)
        .map(|m| m.len() > 2_000_000)
        .unwrap_or(false)
    {
        let _ = fs::rename(&p, p.with_extension("log.1"));
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Expand ~ and %VAR% / $VAR, normalise separators.
pub fn expand(p: &str) -> PathBuf {
    if p.is_empty() {
        return PathBuf::new();
    }
    let mut s = p.to_string();
    if s == "~" || s.starts_with("~/") || s.starts_with("~\\") {
        s = format!("{}{}", home().display(), &s[1..]);
    }
    static EXPAND_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = EXPAND_RE.get_or_init(|| {
        regex::Regex::new(r"%([A-Za-z0-9_]+)%|\$\{?([A-Za-z0-9_]+)\}?").expect("valid expand regex")
    });
    let s = re.replace_all(&s, |c: &regex::Captures| {
        let k = c.get(1).or(c.get(2)).unwrap().as_str();
        std::env::var(k).unwrap_or_else(|_| c[0].to_string())
    });
    let s = if cfg!(windows) {
        s.replace('/', "\\")
    } else {
        s.to_string()
    };
    PathBuf::from(s)
}

pub fn expand_s(p: &str) -> String {
    expand(p).to_string_lossy().to_string()
}

/// Case-insensitive path comparison key (Windows semantics).
pub fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.trim_end_matches('\\').to_string();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

pub fn read_json(path: &Path) -> Option<Value> {
    let raw = fs::read(path).ok()?;
    let txt = String::from_utf8_lossy(&raw);
    let txt = txt.trim_start_matches('\u{feff}'); // tolerate a BOM (files saved by Windows tools)
    serde_json::from_str(txt).ok()
}

pub fn read_json_or(path: &Path, default: Value) -> Value {
    read_json(path).filter(|v| !v.is_null()).unwrap_or(default)
}

pub fn write_json(path: &Path, v: &Value) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let payload = serde_json::to_vec_pretty(v)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let pid = std::process::id();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let fname = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "data.json".into());
    let tmp = path.with_file_name(format!(".{fname}.{pid}.{seq}.tmp"));

    let write_res = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(&payload)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // On Windows, fs::rename over an active reader/writer can briefly fail with
    // ERROR_ACCESS_DENIED (5) or ERROR_SHARING_VIOLATION (32). Retry with short backoff.
    let mut last_err = None;
    for attempt in 0..8 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retryable = matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AlreadyExists
                ) || matches!(e.raw_os_error(), Some(5 | 32));
                last_err = Some(e);
                if !retryable || attempt == 7 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5 * (attempt as u64 + 1)));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(last_err.unwrap_or_else(|| std::io::Error::other("write_json rename failed")))
}

/// ISO-8601 timestamp -> epoch seconds.
pub fn iso_ts(v: &Value) -> Option<f64> {
    let s = v.as_str()?;
    if s.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00"))
        .ok()
        .map(|d| d.timestamp_millis() as f64 / 1000.0)
}

/// Local midnight of the day containing ts.
pub fn midnight(ts: f64) -> f64 {
    use chrono::{Local, TimeZone};
    let dt = Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(Local::now);
    let d = dt.date_naive().and_hms_opt(0, 0, 0).unwrap();
    Local
        .from_local_datetime(&d)
        .earliest()
        .map(|x| x.timestamp() as f64)
        .unwrap_or(ts)
}

pub fn local_hour(ts: f64) -> usize {
    use chrono::{Local, TimeZone, Timelike};
    Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.hour() as usize)
        .unwrap_or(0)
}

pub fn local_date(ts: f64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Read a secret from the environment (Windows: also the user's registry environment, for values set with setx).
pub fn secret(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        if let Ok(k) = winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey("Environment") {
            if let Ok(v) = k.get_value::<String, _>(name) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// GET (body None) or POST JSON, returning JSON. headers: [(name, value)].
pub fn http_json(
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
    timeout_s: u64,
) -> Result<Value, String> {
    request_json(url, headers, body, timeout_s, false)
}

/// A POST to MetaBrain's local API, whose error bodies contain a user-facing message.
pub fn local_post_json(url: &str, body: &Value, timeout_s: u64) -> Result<Value, String> {
    request_json(url, &[], Some(body), timeout_s, true)
}

fn error_code(value: &Value) -> Option<&str> {
    value["error"]["code"]
        .as_str()
        .or_else(|| value["error"]["type"].as_str())
        .or_else(|| value["error"]["status"].as_str())
        .or_else(|| value["code"].as_str())
        .or_else(|| value["type"].as_str())
}

fn http_action(code: u16, reason: Option<&str>) -> &'static str {
    match (code, reason) {
        (402, _)
        | (_, Some("insufficient_balance" | "insufficient_quota" | "insufficient_funds")) => {
            "Saldo o cuota insuficiente. Recarga o revisa el plan del proveedor."
        }
        (408 | 504, _) | (_, Some("timeout" | "request_timeout")) => {
            "El proveedor tardó demasiado en responder. Vuelve a intentar."
        }
        (529, _) | (_, Some("overloaded_error" | "overloaded")) => {
            "El proveedor está sobrecargado temporalmente. Espera e intenta de nuevo."
        }
        (_, Some("rate_limit_exceeded" | "rate_limit")) => {
            "Límite de peticiones alcanzado. Revisa la cuota o espera antes de volver a intentar."
        }
        (_, Some("invalid_api_key" | "authentication_error" | "invalid_key")) => {
            "Clave o sesión inválida. Verifica la clave o vuelve a conectar la cuenta."
        }
        (_, Some("model_not_found" | "model_not_available")) => {
            "El modelo no está disponible. Selecciona un modelo compatible."
        }
        (_, Some("context_length_exceeded")) => {
            "El contexto supera el límite. Acorta el mensaje o inicia otro hilo."
        }
        (_, Some("unsupported_parameter" | "unsupported_value")) => {
            "El modelo rechazó una opción. Revisa el modelo y sus ajustes."
        }
        (401 | 403, _) => "Verifica la clave, la conexión de la cuenta y el acceso al modelo.",
        (404, _) => "Verifica el modelo y la dirección del proveedor.",
        (429, _) => "Revisa la cuota o espera antes de volver a intentar.",
        (400 | 422, _) => "El proveedor rechazó la solicitud. Revisa el modelo y sus ajustes.",
        (500..=599, _) => "El proveedor tiene un problema temporal. Vuelve a intentar.",
        _ => "Revisa la configuración del proveedor.",
    }
}

fn request_json(
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
    timeout_s: u64,
    read_error: bool,
) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(timeout_s))
        .redirects(0)
        .build();
    let mut req = if body.is_some() {
        agent.post(url)
    } else {
        agent.get(url)
    };
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let resp = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => {
            let txt = r.into_string().map_err(|e| e.to_string())?;
            if txt.trim().is_empty() {
                Ok(json!({}))
            } else {
                serde_json::from_str(&txt).map_err(|e| e.to_string())
            }
        }
        Err(ureq::Error::Status(code, response)) => {
            if read_error {
                if let Ok(value) = serde_json::from_reader::<_, Value>(response.into_reader()) {
                    if let Some(message) = value.os("error").filter(|s| !s.trim().is_empty()) {
                        return Err(message);
                    }
                }
            } else if body.is_some() {
                let value = serde_json::from_reader::<_, Value>(std::io::Read::take(
                    response.into_reader(),
                    16_384,
                ))
                .unwrap_or_default();
                let reason = error_code(&value);
                let action = http_action(code, reason);
                return Err(format!("HTTP {code}. {action}"));
            }
            Err(format!("HTTPError {code}"))
        }
        Err(e) => Err(format!("{}", e.kind())),
    }
}

pub fn model_family(model: &str) -> &'static str {
    let m = model.to_lowercase();
    for f in ["opus", "fable", "mythos", "sonnet", "haiku"] {
        if m.contains(f) {
            return f;
        }
    }
    for f in [
        "deepseek", "gpt", "o1", "o3", "o4", "llama", "qwen", "gemma", "mistral", "phi", "kimi",
        "glm",
    ] {
        if m.contains(f) {
            return match f {
                "llama" | "qwen" | "gemma" | "mistral" | "phi" => "local",
                "o1" | "o3" | "o4" | "gpt" => "gpt",
                "deepseek" => "deepseek",
                "kimi" => "kimi",
                _ => "glm",
            };
        }
    }
    "other"
}

// ---------- tiny JSON accessors (config and state are free-form JSON, like the Python version) ----------
pub trait J {
    fn s(&self, k: &str) -> String;
    fn os(&self, k: &str) -> Option<String>;
    fn f(&self, k: &str) -> Option<f64>;
    fn b(&self, k: &str, def: bool) -> bool;
    fn o(&self, k: &str) -> Value;
    fn arr(&self, k: &str) -> Vec<Value>;
}
impl J for Value {
    fn s(&self, k: &str) -> String {
        self.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
    fn os(&self, k: &str) -> Option<String> {
        self.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }
    fn f(&self, k: &str) -> Option<f64> {
        self.get(k).and_then(|v| v.as_f64())
    }
    fn b(&self, k: &str, def: bool) -> bool {
        self.get(k).and_then(|v| v.as_bool()).unwrap_or(def)
    }
    fn o(&self, k: &str) -> Value {
        match self.get(k) {
            Some(v @ Value::Object(_)) => v.clone(),
            _ => Value::Object(Map::new()),
        }
    }
    fn arr(&self, k: &str) -> Vec<Value> {
        self.get(k)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }
}

/// JSON truthiness like Python's `bool()`.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Make a Command hidden (Windows: no console flash).
pub fn hidden(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

/// Strict allowlist for IDs used in filenames and URL path segments (`^[A-Za-z0-9_-]{1,64}$`).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// RAII handle that terminates an entire child process tree when dropped.
#[cfg(windows)]
pub struct ProcessJob {
    _handle: std::os::windows::io::OwnedHandle,
}

#[cfg(not(windows))]
pub struct ProcessJob {
    pid: i32,
}

#[cfg(not(windows))]
impl Drop for ProcessJob {
    fn drop(&mut self) {
        if self.pid > 0 {
            let pgid = format!("-{}", self.pid);
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pgid])
                .status();
        }
    }
}

/// Attach a spawned child process to an OS kill-on-close Job Object (Windows) or process group (Unix).
pub fn attach_kill_job(child: &std::process::Child) -> std::io::Result<ProcessJob> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let raw = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if raw.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = OwnedHandle::from_raw_handle(raw);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(raw, child.as_raw_handle()) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(ProcessJob { _handle: job })
        }
    }
    #[cfg(not(windows))]
    {
        Ok(ProcessJob {
            pid: child.id() as i32,
        })
    }
}

/// Cross-process OS advisory/mandatory file lock (`LockFileEx` on Windows, `flock` on Unix).
/// Automatically released by the OS kernel on drop or process crash with no stale-lock race.
pub struct FileLock {
    _file: fs::File,
}

impl FileLock {
    pub fn acquire(target: &Path, timeout_ms: u64) -> Option<FileLock> {
        let fname = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let lock_path = target.with_file_name(format!(".{fname}.lock"));
        if let Some(parent) = lock_path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .ok()?;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(50));
        loop {
            match file.try_lock() {
                Ok(()) => return Some(FileLock { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(12));
                }
                Err(fs::TryLockError::Error(_)) => return None,
            }
        }
    }
}

/// Global lock for tests that mutate process-wide environment variables (e.g. `METABRAIN_HOME`).
#[cfg(test)]
pub fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// RAII test guard that isolates `METABRAIN_HOME` to a fresh temporary directory and restores it on drop.
#[cfg(test)]
pub struct TestHomeGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<std::ffi::OsString>,
    pub dir: PathBuf,
}

#[cfg(test)]
impl TestHomeGuard {
    pub fn new(tag: &str) -> Self {
        static TEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let lock = test_env_lock();
        let prev = std::env::var_os("METABRAIN_HOME");
        let seq = TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("metabrain_test_{tag}_{}_{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        std::env::set_var("METABRAIN_HOME", &dir);
        Self {
            _lock: lock,
            prev,
            dir,
        }
    }
}

#[cfg(test)]
impl Drop for TestHomeGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("METABRAIN_HOME", v),
            None => std::env::remove_var("METABRAIN_HOME"),
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_post_maps_known_errors_without_exposing_provider_text() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let worker = std::thread::spawn(move || {
            for (status, body) in [
                (
                    400,
                    json!({"error": {"code": "model_not_found", "message": "private response text"}}),
                ),
                (
                    402,
                    json!({"error": {"type": "insufficient_balance", "message": "secret balance"}}),
                ),
                (
                    529,
                    json!({"error": {"type": "overloaded_error", "message": "internal node"}}),
                ),
            ] {
                let req = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                req.respond(
                    tiny_http::Response::from_string(body.to_string()).with_status_code(status),
                )
                .unwrap();
            }
        });
        let error = http_json(&url, &[], Some(&json!({})), 5).unwrap_err();
        assert!(error.contains("HTTP 400") && error.contains("modelo no está disponible"));
        assert!(!error.contains("private response text"));
        let bal = http_json(&url, &[], Some(&json!({})), 5).unwrap_err();
        assert!(bal.contains("HTTP 402") && bal.contains("Saldo o cuota insuficiente"));
        assert!(!bal.contains("secret balance"));
        let ov = http_json(&url, &[], Some(&json!({})), 5).unwrap_err();
        assert!(ov.contains("HTTP 529") && ov.contains("sobrecargado"));
        assert!(!ov.contains("internal node"));
        worker.join().unwrap();
    }

    #[test]
    fn local_post_keeps_the_error_message_returned_by_metabrain() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let message = "La tarea sigue en ejecución.";
        let worker = std::thread::spawn(move || {
            for body in [
                json!({"error": message}).to_string(),
                "Bad Request".to_string(),
            ] {
                let req = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                req.respond(tiny_http::Response::from_string(body).with_status_code(400))
                    .unwrap();
            }
        });

        let body = json!({"text": "Hola"});
        assert_eq!(local_post_json(&url, &body, 5), Err(message.to_string()));
        assert_eq!(
            local_post_json(&url, &body, 5),
            Err("HTTPError 400".to_string())
        );
        worker.join().unwrap();
    }

    #[test]
    fn write_json_handles_concurrent_writers_without_corruption() {
        let dir =
            std::env::temp_dir().join(format!("metabrain_write_json_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("state.json");

        let mut handles = Vec::new();
        for t in 0..6 {
            let p = target.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    write_json(&p, &json!({ "thread": t, "iter": i })).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let parsed = read_json(&target).expect("final file must be valid JSON");
        assert!(parsed.get("thread").is_some());
        assert!(parsed.get("iter").is_some());
        // No orphan .tmp files left behind
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover tmp files: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
