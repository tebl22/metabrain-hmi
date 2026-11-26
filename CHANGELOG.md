# Changelog

Todos los cambios relevantes de MetaBrain HMI se documentan aquí.
El formato sigue [Keep a Changelog](https://keepachangelog.com/es-ES/1.1.0/) y versionado semántico.

## [1.5.0] - 2026-11-25

### Added
- **Cross-platform macOS & Linux support:** Cross-platform `Cargo.toml` dependencies, macOS LaunchAgent and Linux XDG autostart support in `app.rs`, multi-OS GitHub Actions CI/release matrix (Windows, macOS, Linux) in `.github/workflows/rust.yml`, and POSIX `install.sh` (with `--prefix`, `--archive`, `--with-hooks`, `--with-autostart`, and `--uninstall`).
- **Zero-credential `--demo` sandbox (`MetaBrain serve --demo`):** Boots an isolated temporary sandbox with realistic simulated brains, quota rings, tasks, and savings so anyone can test MetaBrain immediately without API keys or CLIs.
- **Clean hook uninstallation:** Added `metabrain-hook hooks uninstall [--dry-run]`, `POST /api/hooks/uninstall`, and a **Uninstall hooks** button in **Settings → Diagnostics** that removes only MetaBrain entries from Claude Code `settings.json` while preserving user-defined hooks.

### Changed
- **100% Unified English Desktop UI:** Completed English translation across all native `egui` pages (`shell`, `home`, `map`, `detail`, `hierarchy`, `brains`, `usage`, `system`, `chat`, `thread`, and `response`), while keeping bilingual Spanish/English directive recognition in `chat_intent.rs`.
- **Documentation:** Updated `README.md` and `README.es.md` with `--demo`, macOS/Linux installation (`install.sh`), and clean hook management.

## [1.4.0] - 2026-11-24

### Security
- **Startup config protection (`app.rs`, `config.rs`):** Removed `Config::from_value` from production startup (gated to `#[cfg(test)]`), validated port range `1..=65535`, preserved `.corrupt.<timestamp>` backups before regenerating corrupted `metabrain.json`, and logged persistence errors.
- **Least-privilege CLI execution (`cli_agents/mod.rs`):** Introduced typed `ExecutionMode::{Plan, Write}`, removed unconditional `danger-full-access` and `--dangerously-skip-permissions` defaults (explicit opt-in via `allow_unsafe_exec` in Settings), unified read-only `plan` mode across Codex, Gemini, and Antigravity, and validated `model`/`cwd` against flag and Windows `.cmd` metacharacter injection.
- **Extensions path & UNC hardening (`extensions.rs`, `server/api.rs`):** Removed environment-variable expansion (`expand_s`) from user-supplied `project` parameters, blocked UNC/network paths, and enforced canonical local directory validation.
- **MCP SSRF / Path Traversal fix (`mcp.rs`):** Enforced strict allowlist (`^[A-Za-z0-9_-]{1,64}$`) on `task_id` before constructing API URLs.
- **Hooks read-only policy hardening (`hooks/policy.rs`):** Fixed bash/PowerShell backslash-quote desyncs, blocked quoted dangerous flags (`find -delete/-exec`, `git -c/--output/--upload-pack/--exec-path`), and blocked `rg --pre` / `--pre-glob`.
- **Proxy, CSP & UI safety (`proxy.rs`, `server/http.rs`, `gui/pages/brains.rs`, `gui/pages/system.rs`):** Disabled upstream HTTP redirects (`redirects(0)` in both proxy and `util::request_json`), capped non-SSE upstream reads at 16 MiB, enforced strict CSP (`default-src 'self'; script-src 'self'`), added `nosniff`/`sandbox` headers on proxy and SSE responses, replaced bare `A`/`D` permission hotkeys with `Ctrl+Shift+A`/`Ctrl+Shift+D`, removed blind `irm | iex` auto-execution, restricted LAN mode to a strict read-only GET allowlist, eliminated `cwd` env-var expansion oracle and `run_claude` `.cmd` injection, hardened `hooks/policy.rs` (`\-flag`, PowerShell `{}`/alias, `git --config-env`/`-o`), added OS kernel file locking (`File::try_lock`), and delimited untrusted upstream worker outputs in parallel prompts.

### Fixed
- **Core metrics & orchestration truth:**
  - Fixed `ledger_task_usage` (`core/chain.rs`) and FATE cache locality `recent_brains_for_cwd` (`parallel/scheduler.rs`) iterating `Value::Array` returned by `TaskManager::list`.
  - Fixed `learn::signal_from_bash` false failures on `"0 failed"` (`cargo test` green output), added `learn::discard_pending` on `fail()`/`stop()`, and populated real candidate chain in `hooks::handlers::shadow_bg`.
  - Fixed `router.rs` cost scale mismatch between USD prices and `OPUS_COST`.
  - Fixed `verify::run_until` unreachable timeout branch (`status?;`), added logical project-name lookup for git worktrees (`detect_for_project`), and enabled automatic verification on parallel `build` branches.
  - Fixed `core/chain.rs` reading active failover account quota, added Antigravity `group == "quota"` support in `brains/api.rs`, fixed `advisor.rs` double-counting `cache_read`, excluded failed/stopped tasks and deduplicated shared `config_dir` in `savings.rs`, fixed `RELAY_MARK` false positives, fixed `Scanner::read_file` offset race and streaming last-chunk accounting, and aligned `handoff::collect_with` with `relay::compact`.
- **Concurrency, processes & UI performance:**
  - Generalized Windows Job Objects / Unix process groups (`util::attach_kill_job`) across CLI runners, moved `cli_agents` stdin delivery onto a background thread to prevent pipe deadlocks, added a 30s external timeout on `agy -p /usage`, fixed `stop()` to terminate child trees before parent handles, and added cross-process `FileLock` in `memory.rs`.
  - Serialized `ParallelEngine::save` per run ID with strict ID validation and unified poisoned-mutex recovery.
  - Moved `prepare_attachments` outside `turn_state` mutex in `tasks/lifecycle.rs`, honored `effort`-only switches, validated OpenAI brain config before queueing, fixed consecutive `user` messages during OpenAI compaction, bounded OpenCode polling to 6s, and fixed `services::watchdog` restart logging.
  - Fixed live chat intent snapshot/revert, `"auto"` reply handling, and `any_avail` calculation (`chat_intent.rs`, `chat.rs`), moved `test_key`/Ollama/`route_task` off the `egui` render thread, debounced Settings `DragValue` writes, and fixed `diff_stats` counting `---` separators.

### Changed
- **100% Pure Rust (zero Python):** Replaced `pdf2md.py` with a native pure-Rust PDF text extractor (`flate2`, supporting uncompressed + FlateDecode streams and ToUnicode CMaps) and removed all legacy Python server/test files.
- **UI Consolidation & English Desktop UI:** Migrated Operation Modes (**Eco / Normal / Max**) and per-brain **Autopilot** into `egui`, updated the primary desktop UI and docs (`README.md`, `SECURITY.md`) to English (with `README.es.md` in Spanish), and replaced the duplicated Web UI with a minimalist, zero-`innerHTML`, read-only mobile/LAN monitor.
- **CI/CD & Installer Hardening:** Updated `.github/workflows/rust.yml` to watch `metabrain/web/**`, enforce `cargo fmt --check`, isolate release permissions, package all runtime assets with full `SHA256SUMS.txt` verification; updated `instalar_rust.ps1` to stop `metabrain-hook.exe`, preserve `--lan`, prune old backups, and harden rollback.

## [1.3.6] - 2026-11-24

- Chat reconoce indicaciones de cerebro y modelo al redactar y permite cambiar el modelo en un hilo.
- Las ramas de una orquestación aparecen como subchats en Chat y como círculos conectados en Inicio.

## [1.3.5] - 2026-11-24

### Added
- Repositorio Git inicializado en el proyecto con tag base `v1.3.1` y `.gitignore` endurecido para excluir respaldos locales (`_respaldo_*/`, `*.bak`).
- Detección de versión de CLIs externos (`codex`, `gemini`, `agy`) expuesta en `cli_agents::cli_version` y reportada en *Diagnóstico* (`insights::doctor`).
- Registro automático (con deduplicación) de eventos JSON desconocidos emitidos por `codex`, `gemini` y `agy` para detectar cambios de formato sin fallos silenciosos.
- Medición real de ahorro por enrutamiento y selección de modelo/esfuerzo en `savings.rs` (combinada con el ahorro exacto por relevos de sesión, con línea base explícita `opus:high` y desglose en la vista de *Uso y ahorro*).
- Pruebas de contrato con fixtures de sesiones reales para `codex`, `gemini` y `antigravity` (`rust/tests/cli_contracts.rs`).
- Prueba de humo de integración del ejecutable real (`rust/tests/smoke.rs`) levantando `MetaBrain serve --no-tray --no-browser` en un puerto temporal y verificando `/api/state`, `/api/tasks`, `/api/savings`, `/api/lan` y `/api/login`.
- El endpoint `/api/lan` ahora informa el estado real de escucha (`listening`, `host`, `tls`) además de la preferencia guardada (`enabled`), y *Ajustes* muestra aviso claro de HTTP sin TLS.
- Lectura en vivo de cuota para **Gemini (Google / Antigravity)** mediante `agy -p /usage` (sesión 5 h y semanal, convirtiendo *Remaining* a porcentaje usado) y corrección de cuota para **ChatGPT (Codex)** (omite snapshots `limit_id: "premium"` con ventanas `null` y compara timestamps reales entre sesiones paralelas).
- Integración de cuotas de `codex` y `antigravity` en la cadena de mando (`Supervisor::chain_entries`), el mapa de cerebros, el panel de detalle y las tarjetas de *Uso y ahorro* mostrando explícitamente cuánto queda (`queda X %`).

### Security & Fixed
- Proxy SSE (`proxy.rs`): corregido el contrato de `std::io::Read` en `SseStream` para reenviar bytes exactos sin corrupción ante buffers pequeños/grandes, evitar OOM con líneas gigantes y propagar errores de estado HTTP y desconexiones.
- Servidor local (`server.rs`): validación estricta de `Host`, `Origin` y Fetch Metadata (`Sec-Fetch-Site`) contra DNS rebinding y CSRF en `localhost`, redacción de secretos en vistas remotas, blindaje de `/api/upload` contra path traversal y carpetas del sistema, y conexión de `POST /api/hooks/install`.
- Hooks y terminal (`hooks.rs`, `connect.rs`, `failover.rs`): endurecido `is_readonly_command` contra inyección de shell y flags destructivos (`find -delete/-exec`, `git -c/--output`); eliminada la interpolación de rutas en `cmd /k` al iniciar sesión con Claude/Codex (pasando `CLAUDE_CONFIG_DIR`/`CODEX_HOME` vía entorno) y validado `session_id`/proyecto al reanudar sesiones.
- Persistencia y cliente HTTP (`util.rs`, `mcp.rs`, `cli_agents/mod.rs`): `write_json` atómico con temporal único (`PID` + contador), `sync_all` y reintentos en Windows ante `ERROR_SHARING_VIOLATION`; cliente HTTP manual de `mcp.rs` reemplazado por `ureq`; corregida lectura de `stderr` en `cli_agents` (`map_while(Result::ok)` + recorte seguro en fronteras UTF-8).
- Calidad y CI: resueltos todos los warnings de `cargo clippy --all-targets`, compilación única de regex con `OnceLock`, y gate estricto `cargo clippy --all-targets --locked -- -D warnings` en `.github/workflows/rust.yml`.

### Changed
- Modularización de `core.rs` en `core/{mod,chain,regulation,actions,tests}.rs` con DTOs tipados (`ChainEntry`, `EffectiveControls`, `AlarmEntry`) y migración de los `static Mutex/OnceLock` de estado (`MODEL_PLAN`, `BUSY`, `PREV`, `LAST`, `SEEN`) a campos de instancia en `Supervisor`.
- Modularización de `tasks.rs` en `tasks/{mod,helpers,lifecycle,runners,tests}.rs` con DTOs tipados (`QueueSummary`, `BrainQueueSlot`, `WaitingEntry`).
- Modularización de `cli_agents.rs` en `cli_agents/{mod,parse,quota,tests}.rs`, tipando con `#[derive(Deserialize)]` los parsers NDJSON de `codex`, `gemini` y `antigravity` y el catálogo/cuotas de Codex, y eliminando `thread_local! ENV` en favor de `env: &[(String, String)]` explícito en `run_blocking_until` y `logged_in_with_env`.
- Modularización de `parallel.rs` en `parallel/{mod,planner,scheduler,tests}.rs` con deserialización tipada para el planificador DAG y respuestas de DeepSeek.
- Modularización de `gui/home.rs` en `gui/home/{mod,map,detail,hierarchy}.rs` con vistas tipadas del snapshot (`HomeSnap`, `BrainSnap`, `ChainSnap`, etc.), eliminando los accesos `.s("...")` del mapa y paneles de *Inicio*.
- Modularización de los 3 monolitos restantes en `hooks/{mod,resolve,policy,handlers,install,tests}.rs`, `brains/{mod,scanner,api,claude,tests}.rs` y `server/{mod,security,auth,http,api,tests}.rs` preservando la API pública y la cobertura de pruebas.
- `insights::setup_toast_identity` ahora usa la API nativa `winreg` en lugar de invocar subprocesos `reg.exe`, reduciendo falsos positivos heurísticos en antivirus (p. ej. Kaspersky).
- `instalar_rust.ps1` prepara la instalación en una carpeta temporal, exige el cierre previo salvo con `-Force` y restaura la instalación anterior si la nueva instancia no confirma su PID en `/api/health`.
- El workflow de CI (`.github/workflows/rust.yml`) ahora ejecuta tanto las pruebas unitarias como las pruebas de integración y de contrato.

## [1.3.4] - 2026-11-24

- El mapa muestra dos anillos concéntricos de uso por cerebro: sesión de 5 h en el exterior y semana en el interior, con ambos porcentajes en el centro.

## [1.3.3] - 2026-11-24

### Changed
- Auto compara candidatos de Claude Code, Codex, Gemini CLI, Antigravity y API de texto según la tarea. Los turnos siguientes de una conversación iniciada en Auto vuelven a evaluar la elección y pueden conservar la sesión actual.
- El motor paralelo favorece harnesses distintos, comparte un límite de contexto entre dependencias y exige una sola implementación final que recibe el pedido completo.
- El panel distingue el ahorro estimado por relevos del equivalente de costo frente a Opus. El uso reportado por el planificador se contabiliza.

### Fixed
- Las continuaciones de los CLI respetan los cupos de concurrencia. El motor paralelo comprueba la disponibilidad antes de marcar un nuevo turno como activo.
- Las continuaciones y escalados conservan los datos del worktree y comprueban su rama antes de editar. Las imágenes se enrutan a los CLI porque el transporte API de tareas solo admite texto.

## [1.3.2] - 2026-11-24

### Fixed
- Si falla el aislamiento solicitado, el agente no se ejecuta en el repositorio original. Los turnos posteriores también requieren el worktree.
- La aprobación se registra solo desde revisión y exige confirmar las ediciones del worktree antes de indicar cómo integrar la rama.
- Las ramas paralelas omiten dependencias fallidas y los resultados parciales se conservan con estado fallido.
- La prueba del hook Python de presupuesto usa una instantánea vigente.
- El instalador solicita cerrar MetaBrain, prepara todos los archivos aparte y recupera la versión anterior si el nuevo servidor no confirma su arranque.
- El panel por LAN queda en modo de solo lectura. La API proxy y las acciones de escritura requieren una conexión desde el equipo local.
- El historial de tareas indica los permisos amplios de Codex en Windows y de Antigravity en modo Construir.
- Los errores al guardar el estado, los eventos o la recuperación de tareas se registran en el log.

## [1.3.1] - 2026-11-23

### Changed
- Modularización de la interfaz gráfica (`pages.rs` dividido en `gui/pages/{mod,chat,thread,brains,usage,system}.rs`).
- Limpieza de archivos `.bak`, carpetas de respaldo antiguas y logs temporales (~118 MB liberados).
- Selección automática de modelo y nivel de esfuerzo para Codex y mejoras de continuidad/borradores en el chat.
