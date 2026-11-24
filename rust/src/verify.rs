//! Verification: an objective check that a task's result works (tests, build, type check).
//!
//! Routing research is consistent on one point: deciding by the request text alone is unreliable,
//! while trying the cheap model first and escalating only when an executable check fails gets
//! close to the best possible cost/quality. This module finds that check for a project and runs it.
use crate::util::{hidden, J};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How a project is verified.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    /// Shown to the user, e.g. "cargo test".
    pub label: String,
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Folder to run it in (where the manifest lives).
    pub dir: PathBuf,
}

/// The project's check: an explicit `"verify": {"<project folder>": "<command>"}` in the config
/// wins (an empty string turns verification off); otherwise one is detected from the files.
pub fn detect(dir: &Path, cfg: &Value) -> Option<Check> {
    detect_for_project(dir, None, cfg)
}

/// Like `detect`, accepting an explicit logical `project_name` (needed when `dir` is a git worktree
/// whose folder name is a hex task ID instead of the repository name).
pub fn detect_for_project(dir: &Path, project_name: Option<&str>, cfg: &Value) -> Option<Check> {
    let project = project_name
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default();
    if let Some(cmd) = cfg
        .get("verify")
        .and_then(|v| v.get(&project))
        .and_then(|v| v.as_str())
    {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return None;
        }
        let argv = if cfg!(windows) {
            vec!["cmd".into(), "/d".into(), "/c".into(), cmd.into()]
        } else {
            vec!["sh".into(), "-c".into(), cmd.into()]
        };
        return Some(Check {
            label: cmd.to_string(),
            argv,
            dir: dir.to_path_buf(),
        });
    }
    detect_files(dir)
}

fn has(dir: &Path, f: &str) -> bool {
    dir.join(f).is_file()
}

/// Detection by manifest, looking in the folder and in its direct children (monorepos such as
/// `rust/Cargo.toml`).
pub fn detect_files(dir: &Path) -> Option<Check> {
    let mut dirs = vec![dir.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut subs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                let n = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                !n.starts_with('.')
                    && !matches!(
                        n.as_str(),
                        "node_modules" | "target" | "dist" | "build" | "venv"
                    )
            })
            .collect();
        subs.sort();
        dirs.extend(subs);
    }
    for d in dirs {
        if let Some(c) = detect_one(&d) {
            return Some(c);
        }
    }
    None
}

fn detect_one(d: &Path) -> Option<Check> {
    let mk = |label: &str, argv: &[&str]| Check {
        label: label.to_string(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        dir: d.to_path_buf(),
    };
    if has(d, "Cargo.toml") {
        // One thread: test suites that touch shared state (env, temp dirs) stay deterministic.
        return Some(mk(
            "cargo test",
            &["cargo", "test", "--quiet", "--", "--test-threads=1"],
        ));
    }
    if has(d, "package.json") {
        let pkg: Value = std::fs::read_to_string(d.join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let scripts = pkg.get("scripts").cloned().unwrap_or(Value::Null);
        let test = scripts.s("test");
        let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
        if !test.is_empty() && !test.contains("no test specified") {
            return Some(mk("npm test", &[npm, "test", "--silent"]));
        }
        if !scripts.s("build").is_empty() {
            return Some(mk("npm run build", &[npm, "run", "build", "--silent"]));
        }
    }
    let py = has(d, "pytest.ini")
        || has(d, "pyproject.toml")
        || has(d, "setup.cfg")
        || d.join("tests").is_dir();
    if py
        && std::fs::read_dir(d)
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.path().extension().map(|x| x == "py").unwrap_or(false))
            })
            .unwrap_or(false)
        || (py && d.join("tests").is_dir() && has_py(&d.join("tests")))
    {
        let python = if cfg!(windows) { "python" } else { "python3" };
        return Some(mk("pytest", &[python, "-m", "pytest", "-q", "-x"]));
    }
    if has(d, "go.mod") {
        return Some(mk("go test", &["go", "test", "./..."]));
    }
    None
}

fn has_py(d: &Path) -> bool {
    std::fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().map(|x| x == "py").unwrap_or(false))
        })
        .unwrap_or(false)
}

/// Result of running a check.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub ok: bool,
    /// Last part of the output: what a model needs to fix the failure.
    pub tail: String,
    pub secs: u64,
}

#[cfg(windows)]
fn verification_job(
    child: &std::process::Child,
) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::*;
    // Closing this handle ends descendants which still own output pipe handles.
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
        Ok(job)
    }
}

fn read_tail(mut input: impl Read) -> String {
    let mut last = Vec::new();
    let mut buf = [0u8; 8192];
    while let Ok(n) = input.read(&mut buf) {
        if n == 0 {
            break;
        }
        last.extend_from_slice(&buf[..n]);
        if last.len() > 16384 {
            last.drain(..last.len() - 16384);
        }
    }
    String::from_utf8_lossy(&last).into_owned()
}

/// Run the check, at most `timeout_s` seconds. A check that cannot start counts as not run (None),
/// never as a failure of the model's work.
pub fn run(c: &Check, timeout_s: u64) -> Option<Outcome> {
    run_until(c, timeout_s, || false)
}

pub fn run_until(c: &Check, timeout_s: u64, cancelled: impl Fn() -> bool) -> Option<Outcome> {
    if cancelled() {
        return None;
    }
    let (prog, args) = c.argv.split_first()?;
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .current_dir(&c.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hidden(&mut cmd);
    let t0 = Instant::now();
    let mut child = cmd.spawn().ok()?;
    #[cfg(windows)]
    let job = match verification_job(&child) {
        Ok(job) => Some(job),
        Err(_) if child.try_wait().ok().flatten().is_some() => None,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let out = child.stdout.take()?;
    let err = child.stderr.take()?;
    let h1 = std::thread::spawn(move || read_tail(out));
    let h2 = std::thread::spawn(move || read_tail(err));
    let deadline = t0 + Duration::from_secs(timeout_s.max(5));
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if Instant::now() < deadline && !cancelled() => {
                std::thread::sleep(Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    #[cfg(windows)]
    drop(job);
    if cancelled() {
        return None;
    }
    let join_deadline = Instant::now() + Duration::from_secs(2);
    while !(h1.is_finished() && h2.is_finished()) && Instant::now() < join_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let out_s = if h1.is_finished() {
        h1.join().unwrap_or_default()
    } else {
        String::new()
    };
    let err_s = if h2.is_finished() {
        h2.join().unwrap_or_default()
    } else {
        String::new()
    };
    let text = format!("{out_s}\n{err_s}");
    let secs = t0.elapsed().as_secs();
    match status {
        Some(st) => Some(Outcome {
            ok: st.success(),
            tail: tail(&text, 3500),
            secs,
        }),
        None => Some(Outcome {
            ok: false,
            tail: format!(
                "Verification timed out after {timeout_s}s and was terminated.\n{}",
                tail(&text, 2500)
            ),
            secs,
        }),
    }
}

/// The last `max` characters, starting on a line boundary.
pub fn tail(text: &str, max: usize) -> String {
    let t = text.trim();
    let n = t.chars().count();
    if n <= max {
        return t.to_string();
    }
    let cut: String = t.chars().skip(n - max).collect();
    match cut.find('\n') {
        Some(i) => cut[i + 1..].to_string(),
        None => cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mb_verify_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_by_manifest_and_subfolder() {
        let d = tmp("cargo");
        std::fs::create_dir_all(d.join("rust")).unwrap();
        std::fs::write(d.join("rust/Cargo.toml"), "[package]").unwrap();
        let c = detect_files(&d).unwrap();
        assert_eq!(c.label, "cargo test");
        assert!(c.dir.ends_with("rust"));

        let n = tmp("npm");
        std::fs::write(n.join("package.json"), r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1","build":"vite build"}}"#).unwrap();
        assert_eq!(detect_files(&n).unwrap().label, "npm run build");

        let p = tmp("py");
        std::fs::create_dir_all(p.join("tests")).unwrap();
        std::fs::write(p.join("tests/test_a.py"), "def test_a(): pass").unwrap();
        assert_eq!(detect_files(&p).unwrap().label, "pytest");

        let e = tmp("none");
        assert!(detect_files(&e).is_none());
    }

    #[test]
    fn config_overrides_and_can_disable() {
        let d = tmp("ovr");
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        let name = d.file_name().unwrap().to_string_lossy().to_string();
        let off = json!({"verify": {name.clone(): ""}});
        assert!(detect(&d, &off).is_none());
        let custom = json!({"verify": {name: "make check"}});
        assert_eq!(detect(&d, &custom).unwrap().label, "make check");
    }

    #[cfg(unix)]
    #[test]
    fn runs_and_reports_pass_fail_and_tail() {
        let d = tmp("run");
        let ok = Check {
            label: "ok".into(),
            argv: vec!["sh".into(), "-c".into(), "echo bien".into()],
            dir: d.clone(),
        };
        let r = run(&ok, 10).unwrap();
        assert!(r.ok);
        assert!(r.tail.contains("bien"));
        let bad = Check {
            label: "bad".into(),
            argv: vec!["sh".into(), "-c".into(), "echo fallo 1>&2; exit 3".into()],
            dir: d.clone(),
        };
        let r = run(&bad, 10).unwrap();
        assert!(!r.ok);
        assert!(r.tail.contains("fallo"));
        let missing = Check {
            label: "x".into(),
            argv: vec!["no-existe-este-programa".into()],
            dir: d,
        };
        assert!(run(&missing, 5).is_none());
    }

    #[test]
    fn tail_keeps_the_end_on_a_line_boundary() {
        let text = (0..100)
            .map(|i| format!("linea {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = tail(&text, 40);
        assert!(t.ends_with("linea 99"));
        assert!(t.starts_with("linea"));
        assert!(t.chars().count() <= 40);
    }
}
