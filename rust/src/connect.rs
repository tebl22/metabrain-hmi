//! Signing a brain in from inside MetaBrain: Claude and ChatGPT accounts open their own
//! sign-in, Google (Antigravity CLI) runs its OAuth flow here (browser + pasted code),
//! API brains get their key pasted, tested and kept in the user's Windows environment.
//! MetaBrain never writes a key into its own files.

use crate::cli_agents::{self, Agent};
use crate::util::{data_dir, expand, hidden, read_json, secret, write_json, J};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// Is this brain ready to work, and what to tell the user if not.
pub fn status(b: &Value) -> (bool, String) {
    match b.s("kind").as_str() {
        "claude_code" => {
            let dir = expand(&b.os("config_dir").unwrap_or_else(|| "~/.claude".into()));
            match claude_email(&dir) {
                Some(e) if !e.is_empty() => (true, format!("Sesión iniciada: {e}")),
                Some(_) => (true, "Sesión iniciada".into()),
                None => (
                    false,
                    "Falta iniciar sesión en esta cuenta de Claude".into(),
                ),
            }
        }
        "openai" | "anthropic" => {
            let k = b.s("key_env");
            if secret(&k).is_some() {
                (true, "Clave guardada".into())
            } else {
                (false, "Falta la clave de API".into())
            }
        }
        "antigravity" => {
            if cli_agents::find_bin(Agent::Antigravity).is_none() {
                (false, "Antigravity CLI no está instalado".into())
            } else if cli_agents::logged_in(Agent::Antigravity) {
                (true, "Sesión de Google iniciada".into())
            } else {
                (false, "Falta iniciar sesión con Google".into())
            }
        }
        "codex" => {
            let home = cli_agents::codex_home(b);
            if cli_agents::find_bin(Agent::Codex).is_none() {
                (false, "Codex CLI no está instalado".into())
            } else {
                match cli_agents::codex_account(&home) {
                    Some(e) if !e.is_empty() => (true, format!("Sesión iniciada: {e}")),
                    Some(_) => (true, "Sesión de ChatGPT iniciada".into()),
                    None => (false, "Falta iniciar sesión con ChatGPT".into()),
                }
            }
        }
        _ => (true, "Listo".into()),
    }
}

/// The account e-mail when this Claude config folder is signed in.
pub fn claude_email(dir: &std::path::Path) -> Option<String> {
    let cred = read_json(&dir.join(".credentials.json"))?;
    cred.get("claudeAiOauth")?.os("accessToken")?;
    let is_main = dir.file_name().map(|n| n == ".claude").unwrap_or(false);
    let acc = if is_main {
        dir.parent().map(|p| p.join(".claude.json"))
    } else {
        Some(dir.join(".claude.json"))
    };
    Some(
        acc.and_then(|a| read_json(&a))
            .map(|v| v.o("oauthAccount").s("emailAddress"))
            .unwrap_or_default(),
    )
}

fn is_safe_cmd_path(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    !s.is_empty()
        && !s.chars().any(|c| {
            matches!(
                c,
                '"' | '&' | '|' | '<' | '>' | '^' | '%' | '!' | '\r' | '\n' | '\0'
            )
        })
}

#[cfg(windows)]
fn build_claude_login_cmd(dir: &std::path::Path) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.env("CLAUDE_CONFIG_DIR", dir).args([
        "/c",
        "start",
        "Iniciar sesión en Claude",
        "cmd",
        "/k",
        "echo Elige tu cuenta y sigue los pasos. Al terminar, cierra esta ventana. && claude",
    ]);
    cmd
}

#[cfg(windows)]
fn build_codex_login_cmd(home: &std::path::Path, bin: &std::path::Path) -> Result<Command, String> {
    if !is_safe_cmd_path(bin) {
        return Err("La ruta del ejecutable de Codex contiene caracteres no permitidos.".into());
    }
    let mut cmd = Command::new("cmd");
    cmd.env("CODEX_HOME", home)
        .args(["/c", "start", "Iniciar sesión en ChatGPT", "cmd", "/k"])
        .arg(format!("\"{}\" login", bin.display()));
    Ok(cmd)
}

/// Open a terminal running Claude Code for that account folder; its first run asks to sign in.
pub fn open_claude_login(config_dir: &str) -> Result<(), String> {
    let dir = expand(config_dir);
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(windows)]
    {
        build_claude_login_cmd(&dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("No se pudo abrir la terminal: {e}"))
    }
    #[cfg(not(windows))]
    {
        Command::new("claude")
            .env("CLAUDE_CONFIG_DIR", &dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// ChatGPT: `codex login` in that brain's own account folder (CODEX_HOME), so each ChatGPT
/// brain can be a different account. It opens the browser by itself.
pub fn open_codex_login(codex_home: &str) -> Result<(), String> {
    let bin = cli_agents::find_bin(Agent::Codex)
        .ok_or("Codex CLI no está instalado. Instálalo con: npm install -g @openai/codex")?;
    let home = expand(codex_home);
    let _ = std::fs::create_dir_all(&home);
    #[cfg(windows)]
    {
        build_codex_login_cmd(&home, &bin)?
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Command::new(bin)
            .arg("login")
            .env("CODEX_HOME", &home)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Keep an API key in the user's environment (Windows: HKCU\Environment, like `setx`) and in
/// this process, so it works right away and after a restart.
pub fn save_key(name: &str, value: &str) -> Result<(), String> {
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("El nombre de la variable solo puede tener letras, números y _".into());
    }
    if value.is_empty() {
        return Err("Pega la clave".into());
    }
    #[cfg(windows)]
    {
        use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
        let env = winreg::RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags("Environment", KEY_SET_VALUE)
            .map_err(|e| format!("No se pudo abrir el entorno de Windows: {e}"))?;
        env.set_value(name, &value.to_string())
            .map_err(|e| format!("No se pudo guardar la clave: {e}"))?;
    }
    std::env::set_var(name, value);
    Ok(())
}

/// Check a key against the provider before saving it. Returns a short, human result.
pub fn test_key(kind: &str, base_url: &str, key: &str) -> Result<String, String> {
    let key = key.trim();
    let base = base_url.trim().trim_end_matches('/');
    let r = if kind == "anthropic" {
        crate::util::http_json(
            &format!("{base}/v1/models"),
            &[("x-api-key", key), ("anthropic-version", "2023-06-01")],
            None,
            20,
        )
    } else {
        crate::util::http_json(
            &format!("{base}/models"),
            &[("Authorization", &format!("Bearer {key}"))],
            None,
            20,
        )
    };
    match r {
        Ok(v) => {
            let n = v.arr("data").len();
            let mut msg = if n > 0 {
                format!("Clave válida · {n} modelos disponibles")
            } else {
                "Clave válida".to_string()
            };
            if base.contains("deepseek.com") {
                if let Ok(b) = crate::util::http_json(
                    &format!("{base}/user/balance"),
                    &[("Authorization", &format!("Bearer {key}"))],
                    None,
                    15,
                ) {
                    if let Some(i) = b.arr("balance_infos").first() {
                        msg += &format!(" · saldo {} {}", i.s("total_balance"), i.s("currency"));
                    }
                }
            }
            Ok(msg)
        }
        Err(e) => {
            let l = e.to_lowercase();
            Err(
                if l.contains("401")
                    || l.contains("403")
                    || l.contains("unauthorized")
                    || l.contains("invalid")
                {
                    "La clave no es válida para este proveedor".into()
                } else {
                    format!("No se pudo comprobar: {e}")
                },
            )
        }
    }
}

// ---------------------------------------------------------------- Google (Antigravity)

#[derive(Clone, Debug, PartialEq)]
pub enum Google {
    Idle,
    Starting,
    /// Browser opened at `url`; waiting for the code Google shows after you accept.
    NeedCode(String),
    Checking,
    Done,
    Failed(String),
}

struct GState {
    state: Google,
    stdin: Option<std::process::ChildStdin>,
}

fn g() -> &'static Mutex<GState> {
    static G: OnceLock<Mutex<GState>> = OnceLock::new();
    G.get_or_init(|| {
        Mutex::new(GState {
            state: Google::Idle,
            stdin: None,
        })
    })
}

pub fn google_state() -> Google {
    g().lock().map(|s| s.state.clone()).unwrap_or(Google::Idle)
}

fn set_state(s: Google) {
    if let Ok(mut x) = g().lock() {
        x.state = s;
    }
}

/// Start Google sign-in. The Antigravity CLI only waits 60 s for sign-in when it runs
/// headless, so sign-in happens in its own console window (no time limit) and we watch
/// for the account to become usable.
pub fn google_start() {
    if matches!(
        google_state(),
        Google::Starting | Google::NeedCode(_) | Google::Checking
    ) {
        return;
    }
    let Some(bin) = cli_agents::find_bin(Agent::Antigravity) else {
        set_state(Google::Failed("Antigravity CLI no está instalado. En PowerShell: irm https://antigravity.google/cli/install.ps1 | iex".into()));
        return;
    };
    // Already signed in? Then there is nothing to open.
    if agy_cred_marker().map(|m| m.exists()).unwrap_or(false) && agy_ok(&bin) {
        let _ = write_json(
            &data_dir().join("agy_login.json"),
            &json!({"ok": true, "ts": crate::util::now()}),
        );
        return set_state(Google::Done);
    }
    set_state(Google::Checking);
    std::thread::spawn(move || {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".into());
        #[cfg(windows)]
        let spawned = {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            Command::new(&bin)
                .current_dir(&home)
                .creation_flags(CREATE_NEW_CONSOLE)
                .spawn()
        };
        #[cfg(not(windows))]
        let spawned = Command::new(&bin).current_dir(&home).spawn();
        if let Err(e) = spawned {
            return set_state(Google::Failed(format!("No se pudo abrir Antigravity: {e}")));
        }
        let marker = agy_cred_marker();
        let before = marker
            .as_ref()
            .and_then(|m| std::fs::metadata(m).and_then(|x| x.modified()).ok());
        // Up to 15 minutes for you to finish in the browser.
        for _ in 0..180 {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if !matches!(google_state(), Google::Checking) {
                return; // cancelled
            }
            let now = marker
                .as_ref()
                .and_then(|m| std::fs::metadata(m).and_then(|x| x.modified()).ok());
            if now.is_some() && now != before && agy_ok(&bin) {
                let _ = write_json(
                    &data_dir().join("agy_login.json"),
                    &json!({"ok": true, "ts": crate::util::now()}),
                );
                return set_state(Google::Done);
            }
        }
        set_state(Google::Failed(
            "No se completó el inicio de sesión. Vuelve a intentarlo.".into(),
        ));
    });
}

/// File the Antigravity CLI rewrites when an account signs in.
fn agy_cred_marker() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA").ok().map(|a| {
        std::path::PathBuf::from(a)
            .join("antigravity")
            .join("credential-user-scope")
    })
}

/// One tiny headless call: true when the signed-in account answers.
fn agy_ok(bin: &std::path::Path) -> bool {
    let mut cmd = Command::new(bin);
    cmd.args([
        "-p",
        "Responde solo: listo",
        "--output-format",
        "json",
        "--print-timeout",
        "45s",
    ])
    .current_dir(std::env::temp_dir())
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let _ = hidden(&mut cmd);
    cmd.output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .map(|v| v.s("status") == "SUCCESS")
        .unwrap_or(false)
}

/// Hand the code Google showed back to the CLI.
pub fn google_code(code: &str) -> Result<(), String> {
    let mut s = g().lock().map_err(|_| "ocupado")?;
    let sin = s
        .stdin
        .as_mut()
        .ok_or("El inicio de sesión ya no está esperando un código. Empieza de nuevo.")?;
    sin.write_all(code.trim().as_bytes())
        .and_then(|_| sin.write_all(b"\n"))
        .and_then(|_| sin.flush())
        .map_err(|e| e.to_string())?;
    s.state = Google::Checking;
    Ok(())
}

pub fn google_reset() {
    set_state(Google::Idle);
}

/// Models the signed-in Google account can use through Antigravity, best first
/// (Gemini 4 Neon for thinking when the plan includes it, Hydrogen for everyday work).
pub fn google_models() -> Vec<String> {
    let Some(bin) = cli_agents::find_bin(Agent::Antigravity) else {
        return vec![];
    };
    let mut cmd = Command::new(bin);
    cmd.arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _ = hidden(&mut cmd);
    let out = match cmd.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return vec![],
    };
    let mut found: Vec<String> = out
        .split(|c: char| c.is_whitespace() || c == ',' || c == '|')
        .map(|w| {
            w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '.')
                .to_lowercase()
        })
        .filter(|w| w.starts_with("gemini-"))
        .collect();
    found.dedup();
    let rank = |m: &str| {
        if m.contains("neon") {
            0
        } else if m.contains("carbon") {
            1
        } else if m.contains("hydrogen") {
            2
        } else {
            3
        }
    };
    found.sort_by_key(|m| rank(m));
    found.dedup();
    found
        .into_iter()
        .filter(|m| m.contains("neon") || m.contains("hydrogen"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn safe_cmd_path_blocks_shell_metacharacters() {
        assert!(is_safe_cmd_path(Path::new(
            r"C:\Users\esteb\AppData\Roaming\npm\codex.cmd"
        )));
        assert!(is_safe_cmd_path(Path::new(
            r"C:\Program Files\nodejs\codex.exe"
        )));
        assert!(!is_safe_cmd_path(Path::new("")));
        assert!(!is_safe_cmd_path(Path::new(r"C:\tmp\a&calc.exe")));
        assert!(!is_safe_cmd_path(Path::new(r#"C:\tmp\a"&&whoami.exe"#)));
        assert!(!is_safe_cmd_path(Path::new(r"C:\tmp\%COMSPEC%")));
        assert!(!is_safe_cmd_path(Path::new("C:\\tmp\\a|b.cmd")));
    }

    #[cfg(windows)]
    #[test]
    fn login_commands_pass_user_dirs_via_env_not_shell_interpolation() {
        let evil_dir = Path::new(r#"C:\Users\test" && calc & echo pwned"#);
        let claude = build_claude_login_cmd(evil_dir);
        let args: Vec<String> = claude
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args
            .iter()
            .any(|a| a.contains("calc") || a.contains("CLAUDE_CONFIG_DIR")));
        assert!(claude
            .get_envs()
            .any(|(k, v)| k == "CLAUDE_CONFIG_DIR" && v == Some(evil_dir.as_os_str())));

        let safe_bin = Path::new(r"C:\Users\esteb\AppData\Roaming\npm\codex.cmd");
        let codex = build_codex_login_cmd(evil_dir, safe_bin).expect("safe bin");
        let cargs: Vec<String> = codex
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!cargs
            .iter()
            .any(|a| a.contains("calc") || a.contains("CODEX_HOME")));
        assert!(codex
            .get_envs()
            .any(|(k, v)| k == "CODEX_HOME" && v == Some(evil_dir.as_os_str())));

        let evil_bin = Path::new(r"C:\tmp\codex&calc.cmd");
        assert!(build_codex_login_cmd(evil_dir, evil_bin).is_err());
    }
}
