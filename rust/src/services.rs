//! Machine vitals and supervised background services (port of metabrain/services.py).
//! Uses sysinfo for processes/CPU/RAM and optional nvidia-smi for the GPU.
use crate::store::Diary;
use crate::util::{expand, log, now, truthy, J};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use sysinfo::System;

const HEAVY: [&str; 2] = ["blender", "ffmpeg"];

#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
    pub cmd: String,
    pub mb: i64,
}

impl Proc {
    pub fn to_json(&self) -> Value {
        json!({"pid": self.pid, "name": self.name, "cmd": self.cmd, "mb": self.mb})
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Gpu {
    pub name: String,
    pub temp: i64,
    pub util: i64,
}

impl Gpu {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "temp": self.temp, "util": self.util})
    }
}

#[derive(Default)]
struct MachineData {
    procs: Vec<Proc>,
    cpu: Option<f64>,
    ram: Option<f64>,
    gpu: Option<Gpu>,
    ts: f64,
}

/// Snapshot of the machine: process list, CPU %, RAM %, GPU and the timestamp of the last poll.
pub struct Machine {
    pub nvsmi: Option<PathBuf>,
    inner: Mutex<MachineData>,
}

impl Machine {
    pub fn new() -> Machine {
        Machine {
            nvsmi: find_nvsmi(),
            inner: Mutex::new(MachineData::default()),
        }
    }

    pub fn poll(&self) {
        // sysinfo needs two samples for a meaningful CPU percentage.
        let mut sys = System::new_all();
        std::thread::sleep(Duration::from_millis(200));
        sys.refresh_all();

        let mut procs = Vec::with_capacity(sys.processes().len());
        for (pid, p) in sys.processes() {
            let name = p.name().to_string_lossy().to_lowercase();
            let cmd = p
                .cmd()
                .iter()
                .map(|a| a.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            procs.push(Proc {
                pid: pid.as_u32(),
                name,
                cmd,
                mb: (p.memory() / 1_048_576) as i64,
            });
        }

        let cpus = sys.cpus();
        let cpu = if cpus.is_empty() {
            0.0
        } else {
            cpus.iter().map(|c| c.cpu_usage() as f64).sum::<f64>() / cpus.len() as f64
        };
        let total = sys.total_memory();
        let used = sys.used_memory();
        let ram = if total > 0 {
            Some((100.0 * used as f64 / total as f64).round())
        } else {
            None
        };

        let gpu = self.query_gpu();
        if let Ok(mut d) = self.inner.lock() {
            d.procs = procs;
            d.cpu = Some(cpu);
            d.ram = ram;
            d.ts = now();
            d.gpu = gpu;
        }
    }

    fn query_gpu(&self) -> Option<Gpu> {
        let exe = self.nvsmi.as_ref()?;
        let mut c = Command::new(exe);
        c.args([
            "--query-gpu=name,temperature.gpu,utilization.gpu",
            "--format=csv,noheader,nounits",
        ]);
        let _ = crate::util::hidden(&mut c);
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = c.spawn().ok()?;
        let _job = crate::util::attach_kill_job(&child).ok();
        let mut stdout = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
            buf
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break None,
            }
        };
        drop(_job);
        let status = status?;
        if !status.success() {
            return None;
        }
        let raw = reader.join().ok()?;
        let text = String::from_utf8_lossy(&raw);
        let line = text.lines().next()?.trim();
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts.len() < 3 {
            return None;
        }
        let temp = parts[1].parse::<i64>().ok()?;
        let util = parts[2].parse::<i64>().ok()?;
        Some(Gpu {
            name: parts[0].to_string(),
            temp,
            util,
        })
    }

    pub fn heavy(&self) -> Vec<Value> {
        let Ok(d) = self.inner.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for p in &d.procs {
            if let Some(kind) = HEAVY.iter().find(|h| p.name.contains(**h)) {
                out.push(json!({"pid": p.pid, "kind": kind, "mb": p.mb}));
            }
        }
        out
    }

    pub fn find(&self, needle: &str) -> Vec<Proc> {
        let n = needle.trim().to_lowercase();
        if n.is_empty() {
            return Vec::new();
        }
        let me = std::process::id();
        let Ok(d) = self.inner.lock() else {
            return Vec::new();
        };
        d.procs
            .iter()
            .filter(|p| {
                p.cmd.to_lowercase().contains(&n)
                    && p.pid != me
                    && !["powershell", "pwsh", "cmd", "ps"]
                        .iter()
                        .any(|s| p.name.starts_with(s))
            })
            .cloned()
            .collect()
    }

    pub fn kill(&self, pid: u32) {
        let pid_s = pid.to_string();
        #[cfg(windows)]
        {
            let mut c = Command::new("taskkill");
            c.args(["/PID", pid_s.as_str(), "/T", "/F"]);
            let _ = crate::util::hidden(&mut c);
            c.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = c.status();
        }
        #[cfg(not(windows))]
        {
            let mut c = Command::new("kill");
            c.args(["-15", pid_s.as_str()]);
            let _ = crate::util::hidden(&mut c);
            c.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = c.status();
        }
    }

    pub fn note_start(&self, cmd: &str) {
        if let Ok(mut d) = self.inner.lock() {
            d.procs.push(Proc {
                pid: 0,
                name: "starting".to_string(),
                cmd: cmd.to_string(),
                mb: 0,
            });
        }
    }

    pub fn remove_pids(&self, pids: &HashSet<u32>) {
        if let Ok(mut d) = self.inner.lock() {
            d.procs.retain(|p| !pids.contains(&p.pid));
        }
    }

    pub fn procs(&self) -> Vec<Proc> {
        self.inner
            .lock()
            .map(|d| d.procs.clone())
            .unwrap_or_default()
    }

    pub fn cpu(&self) -> Option<f64> {
        self.inner.lock().ok().and_then(|d| d.cpu)
    }

    pub fn ram(&self) -> Option<f64> {
        self.inner.lock().ok().and_then(|d| d.ram)
    }

    pub fn gpu(&self) -> Option<Gpu> {
        self.inner.lock().ok().and_then(|d| d.gpu.clone())
    }

    pub fn ts(&self) -> f64 {
        self.inner.lock().map(|d| d.ts).unwrap_or(0.0)
    }
}

impl Default for Machine {
    fn default() -> Self {
        Machine::new()
    }
}

fn find_nvsmi() -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        "nvidia-smi.exe"
    } else {
        "nvidia-smi"
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(exe);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for p in [
        r"C:\Windows\System32\nvidia-smi.exe",
        r"C:\Program Files\NVIDIA Corporation\NVSMI\nvidia-smi.exe",
    ] {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

#[cfg(windows)]
fn find_pythonw() -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let pw = dir.join("pythonw.exe");
            if pw.is_file() {
                return Some(pw);
            }
        }
        for dir in std::env::split_paths(&paths) {
            let py = dir.join("python.exe");
            if py.is_file() {
                let pw = py.with_file_name("pythonw.exe");
                if pw.is_file() {
                    return Some(pw);
                }
            }
        }
    }
    if let Some(lad) = std::env::var_os("LOCALAPPDATA") {
        let pw = PathBuf::from(lad).join(r"Programs\Python\Python312\pythonw.exe");
        if pw.is_file() {
            return Some(pw);
        }
    }
    None
}

fn detach_hidden(cmd: &mut Command) {
    let _ = crate::util::hidden(cmd);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::util::NO_WINDOW | crate::util::DETACHED);
    }
}

fn basename(p: &str) -> String {
    p.rsplit(['/', '\\']).next().unwrap_or(p).to_string()
}

/// Each service: {"id","name","cmd":[...],"cwd","pause_file","heartbeat","autorestart"}.
/// The match string (used to find it among running processes) is the last argument of cmd
/// unless "match" is given.
pub struct Services {
    cfg: std::sync::RwLock<Value>,
    pub m: Arc<Machine>,
    pub diary: Arc<Diary>,
    manual_off: Mutex<HashSet<String>>,
    backoff: Mutex<std::collections::HashMap<String, (u32, f64)>>,
}

impl Services {
    pub fn new(cfg: Value, machine: Arc<Machine>, diary: Arc<Diary>) -> Services {
        Services {
            cfg: std::sync::RwLock::new(cfg),
            m: machine,
            diary,
            manual_off: Mutex::new(HashSet::new()),
            backoff: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn update_cfg(&self, cfg: Value) {
        if let Ok(mut g) = self.cfg.write() {
            *g = cfg;
        }
    }

    pub fn items(&self) -> Vec<Value> {
        let g = self.cfg.read().unwrap_or_else(|e| e.into_inner());
        match g.get("services") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        }
    }

    fn service_id(&self, s: &Value) -> String {
        s.get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn needle(&self, s: &Value) -> String {
        if let Some(m) = s.os("match") {
            return m;
        }
        let cmd = s.arr("cmd");
        match cmd.last() {
            Some(v) => {
                let last = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                basename(&last)
            }
            None => String::new(),
        }
    }

    pub fn running(&self, s: &Value) -> Vec<Proc> {
        if s.arr("cmd").is_empty() {
            return Vec::new();
        }
        self.m.find(&self.needle(s))
    }

    pub fn start(&self, s: &Value) -> String {
        if !self.running(s).is_empty() {
            return "already running".to_string();
        }
        let raw = s.arr("cmd");
        if raw.is_empty() {
            return "no command".to_string();
        }
        #[allow(unused_mut)]
        let mut cmd: Vec<String> = raw
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let c = v
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string());
                if i == 0 {
                    c
                } else {
                    expand(&c).to_string_lossy().to_string()
                }
            })
            .collect();
        #[cfg(windows)]
        {
            if cmd[0] == "python" || cmd[0] == "pythonw" {
                if let Some(pw) = find_pythonw() {
                    cmd[0] = pw.to_string_lossy().into_owned();
                }
            }
        }
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..]);
        let cwd = s.s("cwd");
        if !cwd.is_empty() {
            c.current_dir(expand(&cwd));
        }
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(Value::Object(env)) = s.get("env") {
            for (k, v) in env {
                if let Some(v) = v.as_str() {
                    c.env(k, v);
                }
            }
        }
        detach_hidden(&mut c);
        match c.spawn() {
            Ok(_) => {
                self.m.note_start(&cmd.join(" "));
                "started".to_string()
            }
            Err(e) => {
                log(&format!("service start {}: {e}", cmd[0]));
                format!("failed: {e}")
            }
        }
    }

    pub fn stop(&self, s: &Value) -> String {
        let ps = self.running(s);
        let mut pids = HashSet::new();
        for p in &ps {
            pids.insert(p.pid);
            if p.pid != 0 {
                self.m.kill(p.pid);
            }
        }
        self.m.remove_pids(&pids);
        format!("stopped ({})", ps.len())
    }

    pub fn paused(&self, s: &Value) -> bool {
        let f = expand(&s.s("pause_file"));
        !f.as_os_str().is_empty() && f.exists()
    }

    pub fn set_pause(&self, s: &Value, on: bool, by: &str) -> String {
        let f = expand(&s.s("pause_file"));
        if f.as_os_str().is_empty() {
            return "this service cannot pause".to_string();
        }
        if on {
            if let Some(parent) = f.parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        log(&format!("service pause mkdir {}: {e}", parent.display()));
                    }
                }
            }
            match fs::write(&f, by) {
                Ok(_) => "paused".to_string(),
                Err(e) => {
                    log(&format!("service pause write {}: {e}", f.display()));
                    format!("failed: {e}")
                }
            }
        } else {
            let _ = fs::remove_file(&f);
            "resumed".to_string()
        }
    }

    /// Convenience for the default "manual" author.
    pub fn pause(&self, s: &Value, on: bool) -> String {
        self.set_pause(s, on, "manual")
    }

    pub fn heartbeat(&self, s: &Value) -> (Option<f64>, String) {
        let f = expand(&s.s("heartbeat"));
        let raw = match fs::read(&f) {
            Ok(r) => r,
            Err(_) => return (None, String::new()),
        };
        let text = String::from_utf8_lossy(&raw);
        let txt: String = text.trim().chars().take(140).collect();
        let md = match fs::metadata(&f) {
            Ok(m) => m,
            Err(_) => return (None, String::new()),
        };
        let ts = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64());
        match ts {
            Some(t) => (Some(t), txt),
            None => (None, String::new()),
        }
    }

    pub fn watchdog(&self) {
        let ts = self.m.ts();
        let tnow = now();
        if ts == 0.0 || tnow - ts > 30.0 {
            return;
        }
        for s in self.items() {
            let id = self.service_id(&s);
            let manual = self
                .manual_off
                .lock()
                .map(|g| g.contains(&id))
                .unwrap_or(false);
            let autorestart = s
                .get("autorestart")
                .map(|v| truthy(Some(v)))
                .unwrap_or(true);
            if !self.running(&s).is_empty() {
                if let Ok(mut b) = self.backoff.lock() {
                    b.remove(&id);
                }
                continue;
            }
            if autorestart && !manual {
                let skip = self
                    .backoff
                    .lock()
                    .ok()
                    .and_then(|b| b.get(&id).copied())
                    .is_some_and(|(_, next_try)| tnow < next_try);
                if skip {
                    continue;
                }
                let res = self.start(&s);
                let name = s.get("name").and_then(|v| v.as_str()).unwrap_or(&id);
                if res == "started" || res == "already running" {
                    if let Ok(mut b) = self.backoff.lock() {
                        let fails = b.get(&id).map(|(n, _)| *n).unwrap_or(0);
                        let delay = (20.0 * 2.0_f64.powi(fails.min(4) as i32)).min(300.0);
                        b.insert(id.clone(), (fails.saturating_add(1), tnow + delay));
                    }
                    self.diary.add(
                        "MetaBrain",
                        &format!("{} was down: restarted it", name),
                        "warning",
                    );
                } else {
                    if let Ok(mut b) = self.backoff.lock() {
                        let fails = b.get(&id).map(|(n, _)| *n + 1).unwrap_or(1);
                        let delay = (30.0 * 2.0_f64.powi(fails.min(4) as i32)).min(600.0);
                        b.insert(id.clone(), (fails, tnow + delay));
                    }
                    self.diary.add(
                        "MetaBrain",
                        &format!("{} was down and failed to restart: {}", name, res),
                        "alarm",
                    );
                }
            }
        }
    }

    pub fn snapshot(&self) -> Value {
        let mut out = Vec::new();
        for s in self.items() {
            let id = self.service_id(&s);
            let name = s
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| id.clone());
            let (ts, txt) = self.heartbeat(&s);
            let manual = self
                .manual_off
                .lock()
                .map(|g| g.contains(&id))
                .unwrap_or(false);
            let autorestart = s
                .get("autorestart")
                .map(|v| truthy(Some(v)))
                .unwrap_or(true);
            out.push(json!({
                "id": id,
                "name": name,
                "running": !self.running(&s).is_empty(),
                "paused": self.paused(&s),
                "can_pause": !s.s("pause_file").is_empty(),
                "heartbeat_ts": ts,
                "heartbeat": txt,
                "autorestart": autorestart && !manual
            }));
        }
        Value::Array(out)
    }

    pub fn set_manual_off(&self, id: &str, off: bool) {
        if let Ok(mut g) = self.manual_off.lock() {
            if off {
                g.insert(id.to_string());
            } else {
                g.remove(id);
            }
        }
    }

    pub fn is_manual_off(&self, id: &str) -> bool {
        self.manual_off
            .lock()
            .map(|g| g.contains(id))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "metabrain_svc_{}_{}_{}",
            tag,
            std::process::id(),
            n
        ));
        let _ = fs::create_dir_all(&p);
        p
    }

    #[test]
    fn needle_uses_match_or_last_argument() {
        let s = Services::new(
            json!({"services": []}),
            Arc::new(Machine::new()),
            Arc::new(Diary::new(1)),
        );
        let a = json!({"id": "a", "cmd": ["python", "C:\\jobs\\worker.py"]});
        assert_eq!(s.needle(&a), "worker.py");
        let b = json!({"id": "b", "match": "special task", "cmd": ["x"]});
        assert_eq!(s.needle(&b), "special task");
        let c = json!({"id": "c", "cmd": ["app", "some/script.py"]});
        assert_eq!(s.needle(&c), "script.py");
    }

    #[test]
    fn machine_find_and_heavy() {
        let m = Machine::new();
        let me = std::process::id();
        if let Ok(mut d) = m.inner.lock() {
            d.procs = vec![
                Proc {
                    pid: me,
                    name: "target".into(),
                    cmd: "needle target".into(),
                    mb: 1,
                },
                Proc {
                    pid: 4_000_000_001,
                    name: "powershell.exe".into(),
                    cmd: "powershell needle".into(),
                    mb: 2,
                },
                Proc {
                    pid: 4_000_000_002,
                    name: "blender.exe".into(),
                    cmd: "blender needle.blend".into(),
                    mb: 3,
                },
                Proc {
                    pid: 4_000_000_003,
                    name: "worker.exe".into(),
                    cmd: "python worker.py needle".into(),
                    mb: 4,
                },
                Proc {
                    pid: 4_000_000_004,
                    name: "ffmpeg.exe".into(),
                    cmd: "ffmpeg -i x".into(),
                    mb: 5,
                },
            ];
            d.ts = 10.0;
        }
        let found = m.find("needle");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].pid, 4_000_000_002);
        assert_eq!(found[1].pid, 4_000_000_003);
        let heavy = m.heavy();
        assert_eq!(heavy.len(), 2);
        assert_eq!(
            heavy[0].get("kind").and_then(|v| v.as_str()),
            Some("blender")
        );
        assert_eq!(
            heavy[1].get("kind").and_then(|v| v.as_str()),
            Some("ffmpeg")
        );
    }

    #[test]
    fn pause_roundtrip() {
        let dir = tmpdir("pause");
        let pf = dir.join("pause.flag");
        let svc = json!({"id": "p", "cmd": ["app", "job"], "pause_file": pf.to_string_lossy().to_string()});
        let s = Services::new(
            json!({"services": [svc.clone()]}),
            Arc::new(Machine::new()),
            Arc::new(Diary::new(5)),
        );
        assert!(!s.paused(&svc));
        assert_eq!(s.pause(&svc, true), "paused");
        assert!(s.paused(&svc));
        assert_eq!(fs::read_to_string(&pf).unwrap_or_default(), "manual");
        assert_eq!(s.pause(&svc, false), "resumed");
        assert!(!s.paused(&svc));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn heartbeat_reads_and_truncates() {
        let dir = tmpdir("hb");
        let hb = dir.join("hb.txt");
        let _ = fs::write(&hb, format!("   {}   \n", "x".repeat(200)));
        let svc = json!({"id": "h", "cmd": ["app"], "heartbeat": hb.to_string_lossy().to_string()});
        let s = Services::new(
            json!({"services": []}),
            Arc::new(Machine::new()),
            Arc::new(Diary::new(5)),
        );
        let (ts, txt) = s.heartbeat(&svc);
        assert!(ts.is_some());
        assert_eq!(txt.chars().count(), 140);
        assert!(txt.chars().all(|c| c == 'x'));
        let missing = json!({"id": "m", "cmd": ["app"]});
        let (ts2, txt2) = s.heartbeat(&missing);
        assert!(ts2.is_none());
        assert_eq!(txt2, "");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_fields_and_manual_off() {
        let dir = tmpdir("snap");
        let pf = dir.join("pause.flag");
        let svc = json!({"id": "worker", "name": "Worker", "cmd": ["app", "job"],
                         "pause_file": pf.to_string_lossy().to_string(), "autorestart": true});
        let s = Services::new(
            json!({"services": [svc.clone()]}),
            Arc::new(Machine::new()),
            Arc::new(Diary::new(5)),
        );
        let snap = s.snapshot();
        let a = snap.as_array().cloned().unwrap_or_default();
        assert_eq!(a.len(), 1);
        let row = &a[0];
        assert_eq!(row.get("id").and_then(|v| v.as_str()), Some("worker"));
        assert_eq!(row.get("name").and_then(|v| v.as_str()), Some("Worker"));
        assert_eq!(row.get("running").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(row.get("paused").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(row.get("can_pause").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(row.get("autorestart").and_then(|v| v.as_bool()), Some(true));
        assert!(row
            .get("heartbeat_ts")
            .map(|v| v.is_null())
            .unwrap_or(false));
        assert_eq!(row.get("heartbeat").and_then(|v| v.as_str()), Some(""));

        s.set_manual_off("worker", true);
        let snap2 = s.snapshot();
        let a2 = snap2.as_array().cloned().unwrap_or_default();
        assert_eq!(
            a2[0].get("autorestart").and_then(|v| v.as_bool()),
            Some(false)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_removes_matching_and_watchdog_skips_stale_poll() {
        let m = Arc::new(Machine::new());
        if let Ok(mut d) = m.inner.lock() {
            d.procs.push(Proc {
                pid: 0,
                name: "starting".into(),
                cmd: "x worker.py".into(),
                mb: 0,
            });
        }
        let diary = Arc::new(Diary::new(10));
        let s = Services::new(
            json!({"services": [{"id": "a", "cmd": ["x", "worker.py"]}]}),
            m,
            diary.clone(),
        );
        let svc = json!({"id": "a", "cmd": ["x", "worker.py"]});
        assert_eq!(s.running(&svc).len(), 1);
        assert_eq!(s.stop(&svc), "stopped (1)");
        assert!(s.running(&svc).is_empty());

        // ts == 0 means the machine was never polled: the watchdog must do nothing.
        s.watchdog();
        assert!(diary.last(10).is_empty());
    }
}
