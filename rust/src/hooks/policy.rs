//! Read-only command guard and subagent policy lookup for Claude Code hooks.
use crate::util::J;
use serde_json::{json, Value};

const READONLY_PREFIXES: [&str; 21] = [
    "ls",
    "dir",
    "cat",
    "type",
    "head",
    "tail",
    "pwd",
    "echo",
    "which",
    "where.exe",
    "git status",
    "git diff",
    "git log",
    "git show",
    "grep",
    "rg",
    "find",
    "Get-ChildItem",
    "Get-Content",
    "Select-String",
    "Test-Path",
];

pub(super) fn _subagent_policy(cfg: &Value, bid: &str, agent_type: &str) -> Value {
    let b = cfg
        .arr("brains")
        .into_iter()
        .find(|x| x.get("id").and_then(|v| v.as_str()) == Some(bid))
        .unwrap_or_else(|| json!({}));
    let pol = b.o("subagents");
    let mut p = pol
        .get("*")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if let Some(extra) = pol.get(agent_type).and_then(|v| v.as_object()) {
        for (k, v) in extra {
            p.insert(k.clone(), v.clone());
        }
    }
    Value::Object(p)
}

fn has_shell_metachars(cmd: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = cmd.chars().peekable();
    while let Some(ch) = chars.next() {
        if matches!(ch, '\n' | '\r' | '\0') {
            return true;
        }
        // Backslash escapes differ between bash and PowerShell (where `\` is literal).
        // Reject any backslash before quotes, shell operators, or flag characters (`-`) to prevent
        // parser/shell desync and backslash-escaped flag bypasses (e.g. `\-delete`, `\--pre`).
        if ch == '\\'
            && matches!(
                chars.peek(),
                Some(
                    '\'' | '"'
                        | ';'
                        | '|'
                        | '&'
                        | '>'
                        | '<'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '`'
                        | '$'
                        | '-'
                )
            )
        {
            return true;
        }
        if !in_double && ch == '\'' {
            in_single = !in_single;
            continue;
        }
        if !in_single && ch == '"' {
            in_double = !in_double;
            continue;
        }
        // Command/variable substitution works inside double quotes and unquoted strings.
        if !in_single {
            if ch == '`' {
                return true;
            }
            if ch == '$' && matches!(chars.peek(), Some('(' | '{')) {
                return true;
            }
        }
        // Outside all quotes, reject shell chaining, piping, redirection, subshells, scriptblocks, and backgrounding.
        if !in_single
            && !in_double
            && matches!(ch, ';' | '|' | '&' | '>' | '<' | '(' | ')' | '{' | '}')
        {
            return true;
        }
    }
    // Unclosed quotes are treated conservatively as unsafe.
    in_single || in_double
}

fn unquote_token(tok: &str) -> String {
    tok.chars()
        .filter(|&c| c != '"' && c != '\'' && c != '\\')
        .collect()
}

fn has_dangerous_readonly_flags(lower: &str) -> bool {
    let tokens: Vec<String> = lower.split_whitespace().map(unquote_token).collect();
    let Some(first) = tokens.first().map(|s| s.as_str()) else {
        return true;
    };
    if first == "find" {
        const FIND_BAD: [&str; 9] = [
            "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ];
        if tokens[1..].iter().any(|t| {
            FIND_BAD
                .iter()
                .any(|b| t == b || t.starts_with(&format!("{b}=")))
        }) {
            return true;
        }
    }
    if first == "rg"
        && tokens[1..].iter().any(|t| {
            t == "--pre"
                || t.starts_with("--pre=")
                || t == "--pre-glob"
                || t.starts_with("--pre-glob=")
        })
    {
        return true;
    }
    first == "git"
        && tokens[1..].iter().any(|t| {
            t == "-c"
                || (t.starts_with("-c") && t.len() > 2)
                || t == "--config"
                || t.starts_with("--config=")
                || t == "--config-env"
                || t.starts_with("--config-env=")
                || t == "--output"
                || t.starts_with("--output=")
                || t == "-o"
                || (t.starts_with("-o") && t.len() > 2)
                || t == "--upload-pack"
                || t.starts_with("--upload-pack=")
                || t == "--exec-path"
                || t.starts_with("--exec-path=")
                || t == "--ext-diff"
                || t == "--textconv"
                || t.contains("ext::")
        })
}

pub(super) fn is_readonly_command(cmd: &str) -> bool {
    let c = cmd.trim();
    if c.is_empty() || has_shell_metachars(c) {
        return false;
    }
    let lower = c.to_lowercase();
    let prefix_ok = READONLY_PREFIXES.iter().any(|p| {
        let lp = p.to_lowercase();
        if lower.starts_with(lp.as_str()) {
            lower[lp.len()..]
                .chars()
                .next()
                .map(|ch| ch.is_whitespace())
                .unwrap_or(true)
        } else {
            false
        }
    });
    prefix_ok && !has_dangerous_readonly_flags(&lower)
}

#[cfg(test)]
mod v14_security_tests {
    use super::is_readonly_command;

    #[test]
    fn blocks_escape_desyncs_and_quoted_dangerous_flags() {
        assert!(is_readonly_command("git status --short"));
        assert!(is_readonly_command("rg TODO src/"));
        assert!(!is_readonly_command(r#"echo \' ; rm -rf /"#));
        assert!(!is_readonly_command(r#"echo \" ; calc"#));
        assert!(!is_readonly_command("rg --pre sh pattern"));
        assert!(!is_readonly_command("rg --pre=sh pattern"));
        assert!(!is_readonly_command(r#"find . "-delete""#));
        assert!(!is_readonly_command(r#"find . '-exec' sh -c id \;"#));
        assert!(!is_readonly_command(r#"git "-c" core.pager=sh log"#));
        assert!(!is_readonly_command("git diff --output=/tmp/x"));
        assert!(!is_readonly_command(r"find . \-delete"));
        assert!(!is_readonly_command(r"rg \--pre sh pattern"));
        assert!(!is_readonly_command("where -InputObject 1 { calc }"));
        assert!(!is_readonly_command("git --config-env=core.pager=X log"));
        assert!(!is_readonly_command("git diff -o/tmp/pwn"));
        assert!(!is_readonly_command("git diff --ext-diff"));
    }

    #[test]
    fn extensions_and_ids_reject_traversal_unc_and_env_expansion() {
        assert!(crate::util::valid_id("abc_123-XYZ"));
        assert!(!crate::util::valid_id("../../config#"));
        assert!(!crate::util::valid_id("../other/approve?"));
        assert!(crate::extensions::sanitize_project_dir(r"\\attacker\share").is_err());
        assert!(crate::extensions::sanitize_project_dir(r"C:\proj\%DEEPSEEK_API_KEY%\..").is_err());
        assert!(crate::extensions::sanitize_project_dir("$HOME/proj").is_err());
    }
}
