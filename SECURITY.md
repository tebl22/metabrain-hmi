# Security Policy

## Supported Versions

| Version | Supported |
| ------- | --------- |
| `1.4.x` | Yes |
| `< 1.4` | No |

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report security issues privately via either:
1. **GitHub Security Advisories** (preferred — use the *Report a vulnerability* button in the repository's **Security** tab), or
2. Email: **estebitase@gmail.com** with `[SECURITY] MetaBrain` in the subject line.

Please include reproduction steps, affected component/version, and potential impact. You can expect an initial acknowledgement within 72 hours.

## Security Model & Boundaries

- **Local trust boundary:** MetaBrain binds to `127.0.0.1` by default. Enabling `--lan` exposes a **read-only**, PIN-protected status monitor over plain HTTP (without TLS) and should only be used on trusted private networks.
- **API keys:** Provider keys are read from OS environment variables and are never written into `metabrain.json`.
- **CLI sandboxes:** MetaBrain keeps agent sandboxes enabled by default (`workspace-write` / `plan` read-only). Disabling sandboxes (`allow_unsafe_exec`) is opt-in and explicitly marked as unsafe in Settings.
- **Proxy safety:** Upstream redirects are disabled (`redirects(0)`) to prevent credential forwarding, and non-SSE responses are capped at 16 MiB.
