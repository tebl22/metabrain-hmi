//! Brains page and onboarding wizard.

use crate::gui::theme::*;
use crate::gui::App;
use crate::util::J;
use eframe::egui::{self, Color32, CornerRadius, Margin, Pos2, Sense, Stroke, Vec2};
use serde_json::{json, Value};

// =====================================================================================
// Cerebros
// =====================================================================================

// -------------------------------------------------------------------------------------
// Adding a brain: pick what to connect, then sign in right here.
// -------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Choice {
    #[default]
    None,
    Claude,
    Google,
    ChatGpt,
    DeepSeek,
    GeminiApi,
    OpenRouter,
    Groq,
    OpenAi,
    Anthropic,
    Ollama,
}

type PendingTest =
    std::sync::Arc<std::sync::Mutex<Option<(Result<String, String>, Option<String>)>>>;

#[derive(Clone, Default)]
pub struct Wizard {
    choice: Choice,
    name: String,
    key: String,
    show_key: bool,
    config_dir: String,
    test: Option<Result<String, String>>,
    test_pending: Option<PendingTest>,
    code: String,
    ladder: String,
    tokens_day: String,
    usd_day: String,
    role: String,
    err: String,
    login_opened: bool,
}

struct Spec {
    title: &'static str,
    blurb: &'static str,
    kind: &'static str,
    base: &'static str,
    key_env: &'static str,
    key_page: &'static str,
    ladder: &'static str,
}

fn spec(c: Choice) -> Spec {
    let s = |title, blurb, kind, base, key_env, key_page, ladder| Spec {
        title,
        blurb,
        kind,
        base,
        key_env,
        key_page,
        ladder,
    };
    match c {
        Choice::Claude => s(
            "Cuenta de Claude",
            "Tu suscripción Pro o Max. Inicias sesión en el navegador.",
            "claude_code",
            "",
            "",
            "",
            "claude-opus-5-5, claude-sonnet-5-5, claude-haiku-5-5",
        ),
        Choice::Google => s(
            "Cuenta de Google",
            "Gemini con tu cuenta de Google, sin pagar API.",
            "antigravity",
            "",
            "",
            "",
            "",
        ),
        Choice::ChatGpt => s(
            "Cuenta de ChatGPT",
            "Tu suscripción de ChatGPT, a través de Codex.",
            "codex",
            "",
            "",
            "",
            "gpt-6.2-sol",
        ),
        Choice::DeepSeek => s(
            "DeepSeek",
            "El obrero más barato. Pegas tu clave de API.",
            "openai",
            "https://api.deepseek.com",
            "DEEPSEEK_API_KEY",
            "https://platform.deepseek.com/api_keys",
            "deepseek-flash, deepseek-v4-pro",
        ),
        Choice::GeminiApi => s(
            "Gemini por API",
            "Clave gratuita de Google AI Studio.",
            "openai",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "GEMINI_API_KEY",
            "https://aistudio.google.com/apikey",
            "gemini-3.5-flash, gemini-3.5-flash-lite",
        ),
        Choice::OpenRouter => s(
            "OpenRouter",
            "Cientos de modelos con una sola clave.",
            "openai",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "https://openrouter.ai/keys",
            "",
        ),
        Choice::Groq => s(
            "Groq",
            "Modelos abiertos, muy rápidos.",
            "openai",
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
            "https://console.groq.com/keys",
            "",
        ),
        Choice::OpenAi => s(
            "OpenAI API",
            "Pago por uso con clave de OpenAI.",
            "openai",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            "https://platform.openai.com/api-keys",
            "",
        ),
        Choice::Anthropic => s(
            "Anthropic API",
            "Claude pagando por uso, con clave.",
            "anthropic",
            "https://api.anthropic.com",
            "ANTHROPIC_API_KEY",
            "https://console.anthropic.com/settings/keys",
            "claude-opus-5-5, claude-sonnet-5-5, claude-haiku-5-5",
        ),
        Choice::Ollama => s(
            "Modelos en tu PC",
            "Ollama: gratis y privado, usa tu tarjeta gráfica.",
            "ollama",
            "http://127.0.0.1:11434",
            "",
            "",
            "",
        ),
        Choice::None => s("", "", "", "", "", "", ""),
    }
}

fn is_key_choice(c: Choice) -> bool {
    matches!(
        c,
        Choice::DeepSeek
            | Choice::GeminiApi
            | Choice::OpenRouter
            | Choice::Groq
            | Choice::OpenAi
            | Choice::Anthropic
    )
}

fn slug(s: &str) -> String {
    let b: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    b.trim_matches('-').to_string()
}

pub(super) fn brains(app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    let list = snap.arr("brains");
    let cfg = app.sup.cfg.get();
    ui.horizontal(|ui| {
        ui.label(dim(format!("{} cerebros conectados", list.len())));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if app.ui.wizard.is_none() && primary(ui, "Conectar otro cerebro").clicked() {
                app.ui.wizard = Some(Wizard::default());
            }
        });
    });
    ui.add_space(8.0);
    if app.ui.wizard.is_some() {
        wizard(app, ui, &list);
        ui.add_space(14.0);
    }
    for (i, b) in list.iter().enumerate() {
        let bc = cfg
            .arr("brains")
            .into_iter()
            .find(|x| x.s("id") == b.s("id"))
            .unwrap_or(Value::Null);
        let (ok, msg) = crate::connect::status(&bc);
        card(ui, Some(series(i)), |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(b.s("name")).font(bold(15.0)).color(INK));
                    ui.label(faint(format!(
                        "{} · {}",
                        kind_label(&b.s("kind")),
                        role_label(&b.s("role"))
                    )));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let id = b.s("id");
                    if app.ui.confirm_remove.as_deref() == Some(id.as_str()) {
                        if danger(ui, "Sí, quitar").clicked() {
                            let mut c = app.sup.cfg.get();
                            if let Some(arr) = c.get_mut("brains").and_then(|v| v.as_array_mut()) {
                                arr.retain(|x| x.s("id") != id);
                            }
                            let r = app.sup.act(&json!({"a": "config", "config": c}));
                            app.ui.confirm_remove = None;
                            app.say(
                                if r == "ok" {
                                    format!("Quitado {}", b.s("name"))
                                } else {
                                    r
                                },
                                ATTN,
                            );
                        }
                        if quiet(ui, "Cancelar").clicked() {
                            app.ui.confirm_remove = None;
                        }
                    } else if quiet(ui, "Quitar").clicked() {
                        app.ui.confirm_remove = Some(id.clone());
                    }
                    chip(
                        ui,
                        if ok { "Conectado" } else { "Sin conectar" },
                        if ok { RUN } else { ATTN },
                    );
                });
            });
            ui.add_space(6.0);
            // Connection line with the one action that fixes it.
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(&msg).font(body(12.5)).color(if ok {
                        INK2
                    } else {
                        ATTN
                    }))
                    .wrap(),
                );
                let kind = bc.s("kind");
                let label = match kind.as_str() {
                    "claude_code" => {
                        if ok {
                            "Cambiar de cuenta"
                        } else {
                            "Iniciar sesión"
                        }
                    }
                    "antigravity" => {
                        if ok {
                            ""
                        } else {
                            "Iniciar sesión con Google"
                        }
                    }
                    "codex" => {
                        if ok {
                            ""
                        } else {
                            "Iniciar sesión con ChatGPT"
                        }
                    }
                    "openai" | "anthropic" => {
                        if ok {
                            "Cambiar clave"
                        } else {
                            "Pegar clave"
                        }
                    }
                    _ => "",
                };
                if !label.is_empty()
                    && (if ok {
                        quiet(ui, label)
                    } else {
                        primary(ui, label)
                    })
                    .clicked()
                {
                    match kind.as_str() {
                        "claude_code" => {
                            if let Err(e) = crate::connect::open_claude_login(
                                &bc.os("config_dir").unwrap_or_else(|| "~/.claude".into()),
                            ) {
                                app.say(e, ALARM);
                            } else {
                                app.say("Se abrió el inicio de sesión de Claude", INFO);
                            }
                        }
                        "antigravity" => {
                            app.ui.wizard = Some(Wizard {
                                choice: Choice::Google,
                                name: b.s("name"),
                                ..Default::default()
                            });
                            crate::connect::google_start();
                        }
                        "codex" => {
                            if let Err(e) = crate::connect::open_codex_login(
                                &bc.os("codex_home").unwrap_or_else(|| "~/.codex".into()),
                            ) {
                                app.say(e, ALARM);
                            }
                        }
                        _ => {
                            let choice = guess_choice(&bc);
                            app.ui.wizard = Some(Wizard {
                                choice,
                                name: b.s("name"),
                                ..Default::default()
                            });
                        }
                    }
                }
            });
            ui.add_space(4.0);
            let models: Vec<String> = b
                .arr("models")
                .iter()
                .filter_map(|v| v.as_str().map(crate::gui::home::model_label))
                .collect();
            ui.horizontal_wrapped(|ui| {
                ui.label(faint("Modelos"));
                ui.label(
                    egui::RichText::new(if models.is_empty() {
                        "el que elija el agente".into()
                    } else {
                        models.join("  ›  ")
                    })
                    .font(mono(12.0))
                    .color(INK2),
                );
            });
            let budget = b.get("budget").cloned().unwrap_or(Value::Null);
            ui.horizontal_wrapped(|ui| {
                ui.label(faint("Límite diario"));
                let tok = budget
                    .f("tokens_day")
                    .map(num)
                    .unwrap_or_else(|| "sin límite".into());
                let usd = budget
                    .f("usd_day")
                    .map(|u| format!("{u:.2} USD"))
                    .unwrap_or_else(|| "sin límite".into());
                ui.label(
                    egui::RichText::new(format!("{tok} tokens · {usd}"))
                        .font(mono(12.0))
                        .color(INK2),
                );
            });
            for rec in b
                .get("pilot")
                .map(|p| p.arr("recs"))
                .unwrap_or_default()
                .iter()
                .take(2)
            {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(crate::l10n::es(&rec.s("text")))
                            .font(body(12.5))
                            .color(level_color(&rec.s("level"))),
                    );
                    if quiet(ui, "Aplicar").clicked() {
                        app.act(json!({"a": "apply_rec", "brain": b.s("id"), "rec": rec.s("id")}));
                    }
                });
            }
        });
        ui.add_space(10.0);
    }
}

fn kind_label(k: &str) -> &'static str {
    match k {
        "claude_code" => "Cuenta de Claude",
        "antigravity" => "Cuenta de Google",
        "codex" => "Cuenta de ChatGPT",
        "openai" => "API",
        "anthropic" => "API de Anthropic",
        "ollama" => "En tu PC",
        "gemini_cli" => "Gemini CLI",
        _ => "Cerebro",
    }
}

fn role_label(r: &str) -> &str {
    match r {
        "director" => "director",
        "producer" => "productor",
        "worker" => "obrero",
        "local" => "local",
        x => x,
    }
}

fn guess_choice(b: &Value) -> Choice {
    let u = b.s("base_url");
    if b.s("kind") == "anthropic" {
        Choice::Anthropic
    } else if u.contains("deepseek") {
        Choice::DeepSeek
    } else if u.contains("generativelanguage") {
        Choice::GeminiApi
    } else if u.contains("openrouter") {
        Choice::OpenRouter
    } else if u.contains("groq") {
        Choice::Groq
    } else {
        Choice::OpenAi
    }
}

fn wizard(app: &mut App, ui: &mut egui::Ui, list: &[Value]) {
    let Some(mut w) = app.ui.wizard.clone() else {
        return;
    };
    let mut close = false;
    let mut added = false;
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, MASTER.gamma_multiply(0.5)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if w.choice != Choice::None
                    && icon_button(ui, Icon::Up, "Volver", false, true).clicked()
                {
                    w = Wizard::default();
                    crate::connect::google_reset();
                }
                ui.label(
                    egui::RichText::new(if w.choice == Choice::None {
                        "¿Qué quieres conectar?".to_string()
                    } else {
                        spec(w.choice).title.to_string()
                    })
                    .font(cond(19.0))
                    .color(INK),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, Icon::Close, "Cerrar", false, true).clicked() {
                        close = true;
                    }
                });
            });
            ui.add_space(10.0);
            if w.choice == Choice::None {
                choose(ui, &mut w, list);
            } else {
                added = connect_step(app, ui, &mut w, list);
            }
        });
    app.ui.wizard = if close || added { None } else { Some(w) };
    if matches!(
        app.ui.wizard.as_ref().map(|w| w.choice),
        Some(Choice::Google | Choice::Claude | Choice::ChatGpt)
    ) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(700));
    }
}

fn choose(ui: &mut egui::Ui, w: &mut Wizard, list: &[Value]) {
    let groups: [(&str, &[Choice]); 3] = [
        (
            "Con tu cuenta (sin pagar API)",
            &[Choice::Claude, Choice::Google, Choice::ChatGpt],
        ),
        (
            "Con clave de API (pago por uso, muy barato)",
            &[
                Choice::DeepSeek,
                Choice::GeminiApi,
                Choice::OpenRouter,
                Choice::Groq,
                Choice::OpenAi,
                Choice::Anthropic,
            ],
        ),
        ("En tu computadora", &[Choice::Ollama]),
    ];
    let cols = if ui.available_width() > 900.0 {
        3
    } else if ui.available_width() > 540.0 {
        2
    } else {
        1
    };
    let gap = 10.0;
    let cw = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
    for (title, items) in groups {
        ui.label(egui::RichText::new(title).font(medium(12.5)).color(INK3));
        ui.add_space(4.0);
        for row in items.chunks(cols) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for &c in row {
                    let sp = spec(c);
                    let (r, resp) = ui.allocate_exact_size(Vec2::new(cw, 66.0), Sense::click());
                    let p = ui.painter();
                    p.rect(
                        r,
                        CornerRadius::same(8),
                        if resp.hovered() { RAISED2 } else { RAISED },
                        Stroke::new(
                            1.0,
                            if resp.hovered() {
                                MASTER.gamma_multiply(0.6)
                            } else {
                                RULE
                            },
                        ),
                        egui::StrokeKind::Inside,
                    );
                    let ic = Pos2::new(r.min.x + 30.0, r.center().y);
                    let col = match c {
                        Choice::Claude | Choice::Anthropic => Color32::from_rgb(0xD9, 0x77, 0x57),
                        Choice::Google | Choice::GeminiApi => Color32::from_rgb(0x39, 0x87, 0xE5),
                        Choice::ChatGpt | Choice::OpenAi => Color32::from_rgb(0x19, 0x9E, 0x70),
                        Choice::DeepSeek => Color32::from_rgb(0x4D, 0x6B, 0xFE),
                        _ => INK3,
                    };
                    p.circle_filled(ic, 15.0, col.gamma_multiply(0.22));
                    p.circle_stroke(ic, 15.0, Stroke::new(1.5, col));
                    p.text(
                        ic,
                        egui::Align2::CENTER_CENTER,
                        sp.title.chars().next().unwrap_or('?').to_string(),
                        bold(14.0),
                        INK,
                    );
                    p.text(
                        Pos2::new(r.min.x + 56.0, r.min.y + 22.0),
                        egui::Align2::LEFT_CENTER,
                        sp.title,
                        bold(13.5),
                        INK,
                    );
                    let galley = p.layout(sp.blurb.to_string(), body(12.0), INK3, cw - 68.0);
                    p.galley(Pos2::new(r.min.x + 56.0, r.min.y + 33.0), galley, INK3);
                    if resp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        let n_claude = list.iter().filter(|b| b.s("kind") == "claude_code").count();
                        *w = Wizard {
                            choice: c,
                            ..Default::default()
                        };
                        w.name = match c {
                            Choice::Claude => format!("Claude {}", n_claude + 1),
                            Choice::Google => "Gemini (Google)".into(),
                            Choice::ChatGpt => "ChatGPT".into(),
                            _ => sp.title.into(),
                        };
                        w.config_dir = if c == Choice::ChatGpt {
                            format!("~/.codex-{}", slug(&w.name))
                        } else {
                            format!("~/.claude-{}", slug(&w.name))
                        };
                        w.ladder = sp.ladder.into();
                        w.role = match c {
                            Choice::Claude | Choice::ChatGpt | Choice::Google => "producer".into(),
                            Choice::Ollama => "local".into(),
                            _ => "worker".into(),
                        };
                        if c == Choice::DeepSeek {
                            w.tokens_day = "300000".into();
                            w.usd_day = "1".into();
                        }
                    }
                }
            });
            ui.add_space(gap);
        }
        ui.add_space(4.0);
    }
}

/// Step two: sign in / paste the key. Returns true when the brain was added.
fn connect_step(app: &mut App, ui: &mut egui::Ui, w: &mut Wizard, list: &[Value]) -> bool {
    let sp = spec(w.choice);
    ui.label(dim(sp.blurb));
    ui.add_space(10.0);
    let mut ready = false;

    match w.choice {
        Choice::Claude => {
            let dir = expand_dir(&w.config_dir);
            let email = crate::connect::claude_email(&dir);
            step(
                ui,
                1,
                "Abre el inicio de sesión",
                "Se abre una ventana de Claude Code; elige tu cuenta y acepta en el navegador.",
            );
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                if primary(
                    ui,
                    if w.login_opened {
                        "Abrir de nuevo"
                    } else {
                        "Iniciar sesión en Claude"
                    },
                )
                .clicked()
                {
                    match crate::connect::open_claude_login(&w.config_dir) {
                        Ok(()) => w.login_opened = true,
                        Err(e) => w.err = e,
                    }
                }
            });
            ui.add_space(8.0);
            step(ui, 2, "Vuelve aquí", "MetaBrain detecta la sesión sola.");
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                match &email {
                    Some(e) => {
                        chip(
                            ui,
                            &if e.is_empty() {
                                "Sesión iniciada".to_string()
                            } else {
                                format!("Conectado: {e}")
                            },
                            RUN,
                        );
                    }
                    None => {
                        ui.spinner();
                        ui.label(faint(if w.login_opened {
                            "Esperando a que termines de iniciar sesión…"
                        } else {
                            "Aún sin sesión"
                        }));
                    }
                }
            });
            ready = email.is_some();
        }
        Choice::Google => {
            let installed =
                crate::cli_agents::find_bin(crate::cli_agents::Agent::Antigravity).is_some();
            let st = crate::connect::google_state();
            let logged = crate::cli_agents::logged_in(crate::cli_agents::Agent::Antigravity)
                || st == crate::connect::Google::Done;
            if !installed {
                step(
                    ui,
                    1,
                    "Install Antigravity CLI",
                    "Official Google CLI to use Gemini with your Google account.",
                );
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(30.0);
                    if primary(ui, "Copy install command").clicked() {
                        ui.ctx().copy_text(
                            "irm https://antigravity.google/cli/install.ps1 | iex".to_string(),
                        );
                        w.err =
                            "Command copied. Review and run it in PowerShell, then return here."
                                .into();
                    }
                    if ui.button("Open docs").clicked() {
                        ui.ctx()
                            .open_url(egui::OpenUrl::new_tab("https://antigravity.google/cli"));
                    }
                });
                ui.add_space(8.0);
            }
            step(
                ui,
                if installed { 1 } else { 2 },
                "Inicia sesión con Google",
                "Se abre tu navegador; acepta y Google te muestra un código.",
            );
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                match &st {
                    crate::connect::Google::Idle | crate::connect::Google::Failed(_) if !logged => {
                        if ui
                            .add_enabled(
                                installed,
                                egui::Button::new(
                                    egui::RichText::new("Iniciar sesión con Google")
                                        .font(medium(13.0))
                                        .color(Color32::from_rgb(0x1C, 0x16, 0x0A)),
                                )
                                .fill(MASTER),
                            )
                            .clicked()
                        {
                            crate::connect::google_start();
                        }
                    }
                    crate::connect::Google::Starting => {
                        ui.spinner();
                        ui.label(faint("Abriendo Google…"));
                    }
                    _ => {}
                }
                if let crate::connect::Google::Failed(e) = &st {
                    ui.label(egui::RichText::new(e).color(ALARM));
                }
            });
            if let crate::connect::Google::NeedCode(url) = &st {
                ui.add_space(8.0);
                step(
                    ui,
                    if installed { 2 } else { 3 },
                    "Pega el código",
                    "Cópialo de la página de Google y pégalo aquí.",
                );
                ui.horizontal_wrapped(|ui| {
                    ui.add_space(30.0);
                    let w_in = (ui.available_width() - 180.0).clamp(160.0, 340.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut w.code)
                            .hint_text("4/0Ab…")
                            .desired_width(w_in)
                            .font(mono(13.0)),
                    );
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (primary(ui, "Conectar").clicked() || enter) && !w.code.trim().is_empty() {
                        if let Err(e) = crate::connect::google_code(&w.code) {
                            w.err = e;
                        }
                    }
                    if quiet(ui, "Abrir la página otra vez").clicked() {
                        crate::app::open_browser(url);
                    }
                });
            }
            if st == crate::connect::Google::Checking {
                ui.horizontal(|ui| {
                    ui.add_space(30.0);
                    ui.spinner();
                    ui.label(faint("Se abrió una ventana de Antigravity: elige iniciar sesión con Google y acepta en el navegador. Esto se actualiza solo."));
                    if quiet(ui, "Cancelar").clicked() {
                        crate::connect::google_reset();
                    }
                });
            }
            if logged {
                ui.horizontal(|ui| {
                    ui.add_space(30.0);
                    chip(ui, "Cuenta de Google conectada", RUN);
                });
            }
            ready = logged;
        }
        Choice::ChatGpt => {
            let installed = crate::cli_agents::find_bin(crate::cli_agents::Agent::Codex).is_some();
            let home = expand_dir(&w.config_dir);
            let account = crate::cli_agents::codex_account(&home);
            let existing = crate::cli_agents::codex_account(&crate::util::home().join(".codex"))
                .filter(|_| w.config_dir != "~/.codex");
            if !installed {
                step(
                    ui,
                    1,
                    "Instala Codex",
                    "La herramienta oficial de OpenAI para usar ChatGPT desde aquí.",
                );
                ui.horizontal(|ui| {
                    ui.add_space(30.0);
                    if primary(ui, "Instalar").clicked() {
                        #[cfg(windows)]
                        let _ = std::process::Command::new("cmd")
                            .args([
                                "/c",
                                "start",
                                "Instalar Codex",
                                "cmd",
                                "/k",
                                "npm install -g @openai/codex && echo Listo. Cierra esta ventana.",
                            ])
                            .spawn();
                    }
                });
                ui.add_space(8.0);
            }
            step(
                ui,
                if installed { 1 } else { 2 },
                "Inicia sesión con la cuenta que quieras",
                "Se abre tu navegador; entra con la cuenta de ChatGPT que usará este cerebro.",
            );
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                match &account {
                    Some(e) => {
                        chip(
                            ui,
                            &if e.is_empty() {
                                "Cuenta conectada".to_string()
                            } else {
                                format!("Conectado: {e}")
                            },
                            RUN,
                        );
                    }
                    None => {
                        if ui
                            .add_enabled(
                                installed,
                                egui::Button::new(
                                    egui::RichText::new(if w.login_opened {
                                        "Abrir de nuevo"
                                    } else {
                                        "Iniciar sesión con ChatGPT"
                                    })
                                    .font(medium(13.0))
                                    .color(Color32::from_rgb(0x1C, 0x16, 0x0A)),
                                )
                                .fill(MASTER),
                            )
                            .clicked()
                        {
                            match crate::connect::open_codex_login(&w.config_dir) {
                                Ok(()) => w.login_opened = true,
                                Err(e) => w.err = e,
                            }
                        }
                        if w.login_opened {
                            ui.spinner();
                            ui.label(faint("Esperando a que termines en el navegador…"));
                        }
                    }
                }
            });
            if let (None, Some(e)) = (&account, &existing) {
                ui.horizontal(|ui| {
                    ui.add_space(30.0);
                    if quiet(
                        ui,
                        &format!(
                            "Usar la cuenta que ya está en este PC{}",
                            if e.is_empty() {
                                String::new()
                            } else {
                                format!(" ({e})")
                            }
                        ),
                    )
                    .clicked()
                    {
                        w.config_dir = "~/.codex".into();
                    }
                });
            }
            ready = account.is_some();
        }
        Choice::Ollama => {
            step(
                ui,
                1,
                "Check Ollama",
                "Ollama must be running locally on this PC.",
            );
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                let busy = w.test_pending.is_some();
                if ui
                    .add_enabled(!busy, egui::Button::new("Detect models"))
                    .clicked()
                {
                    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
                    w.test_pending = Some(slot.clone());
                    let base = sp.base.to_string();
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let out =
                            match crate::util::http_json(&format!("{base}/api/tags"), &[], None, 5)
                            {
                                Ok(v) => {
                                    let ms: Vec<String> =
                                        v.arr("models").iter().map(|m| m.s("name")).collect();
                                    (
                                        Ok(format!("{} models found", ms.len())),
                                        Some(ms.join(", ")),
                                    )
                                }
                                Err(_) => (
                                    Err("Could not reach Ollama. Start Ollama and try again."
                                        .into()),
                                    None,
                                ),
                            };
                        if let Ok(mut g) = slot.lock() {
                            *g = Some(out);
                        }
                        ctx.request_repaint();
                    });
                }
                if busy {
                    ui.spinner();
                    ui.label(faint("Checking Ollama…"));
                }
            });
            ready = matches!(w.test, Some(Ok(_)));
        }
        _ if is_key_choice(w.choice) => {
            let have = crate::util::secret(sp.key_env).is_some();
            step(
                ui,
                1,
                "Get your API key",
                "Create a key on the provider's console and copy it.",
            );
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                if quiet(ui, "Open API keys page").clicked() {
                    crate::app::open_browser(sp.key_page);
                }
                if have {
                    ui.label(faint(format!(
                        "A key is already stored in {}; paste a new one only to replace it.",
                        sp.key_env
                    )));
                }
            });
            ui.add_space(8.0);
            step(
                ui,
                2,
                "Paste it here",
                "Saved in your OS user environment, never inside MetaBrain config files.",
            );
            ui.horizontal_wrapped(|ui| {
                ui.add_space(30.0);
                let w_in = (ui.available_width() - 120.0).clamp(160.0, 360.0);
                ui.add(
                    egui::TextEdit::singleline(&mut w.key)
                        .password(!w.show_key)
                        .hint_text("sk-…")
                        .desired_width(w_in)
                        .font(mono(13.0)),
                );
                if icon_button(
                    ui,
                    Icon::Router,
                    if w.show_key { "Hide" } else { "Show" },
                    w.show_key,
                    true,
                )
                .clicked()
                {
                    w.show_key = !w.show_key;
                }
                let busy = w.test_pending.is_some();
                if ui
                    .add_enabled(!busy && !w.key.trim().is_empty(), egui::Button::new("Test"))
                    .clicked()
                {
                    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
                    w.test_pending = Some(slot.clone());
                    let kind = sp.kind.to_string();
                    let base = sp.base.to_string();
                    let key = w.key.clone();
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let res = crate::connect::test_key(&kind, &base, &key);
                        if let Ok(mut g) = slot.lock() {
                            *g = Some((res, None));
                        }
                        ctx.request_repaint();
                    });
                }
                if busy {
                    ui.spinner();
                }
            });
            ready = have || matches!(w.test, Some(Ok(_)));
        }
        _ => {}
    }
    if let Some(slot) = w.test_pending.clone() {
        let done = slot.lock().ok().and_then(|mut g| g.take());
        if let Some((res, maybe_ladder)) = done {
            w.test = Some(res);
            if let Some(l) = maybe_ladder {
                w.ladder = l;
            }
            w.test_pending = None;
        }
    }
    if let Some(t) = &w.test {
        ui.horizontal(|ui| {
            ui.add_space(30.0);
            match t {
                Ok(m) => chip(ui, m, RUN),
                Err(e) => chip(ui, e, ALARM),
            };
        });
    }
    if !w.err.is_empty() {
        ui.label(egui::RichText::new(&w.err).color(ALARM));
    }

    ui.add_space(12.0);
    egui::CollapsingHeader::new(
        egui::RichText::new("Opciones avanzadas")
            .font(medium(12.5))
            .color(INK2),
    )
    .id_salt("adv")
    .show(ui, |ui| {
        let narrow = ui.available_width() < 520.0;
        let field = |ui: &mut egui::Ui, label: &str, v: &mut String, hint: &str| {
            if narrow {
                ui.label(dim(label));
                ui.add(
                    egui::TextEdit::singleline(v)
                        .hint_text(hint)
                        .desired_width(ui.available_width().min(360.0)),
                );
            } else {
                ui.horizontal(|ui| {
                    ui.add_sized([170.0, 24.0], egui::Label::new(dim(label)));
                    let w_in = (ui.available_width() - 12.0).clamp(160.0, 360.0);
                    ui.add(
                        egui::TextEdit::singleline(v)
                            .hint_text(hint)
                            .desired_width(w_in),
                    );
                });
            }
        };
        field(ui, "Nombre", &mut w.name, "");
        ui.horizontal_wrapped(|ui| {
            ui.add_sized([170.0, 24.0], egui::Label::new(dim("Rol")));
            for (r, n) in [
                ("director", "director"),
                ("producer", "productor"),
                ("worker", "obrero"),
                ("local", "local"),
            ] {
                ui.selectable_value(&mut w.role, r.to_string(), n);
            }
        });
        if w.choice == Choice::Claude || w.choice == Choice::ChatGpt {
            field(
                ui,
                "Carpeta de la cuenta",
                &mut w.config_dir,
                if w.choice == Choice::Claude {
                    "~/.claude-gmail"
                } else {
                    "~/.codex-trabajo"
                },
            );
        }
        field(
            ui,
            "Modelos (mejor › barato)",
            &mut w.ladder,
            "el que elija el agente",
        );
        field(ui, "Tokens por día", &mut w.tokens_day, "sin límite");
        field(ui, "USD por día", &mut w.usd_day, "sin límite");
    });
    ui.add_space(12.0);

    let mut added = false;
    ui.horizontal(|ui| {
        let label = if ready {
            "Agregar cerebro"
        } else {
            "Agregar sin conectar"
        };
        let r = if ready {
            primary(ui, label)
        } else {
            quiet(ui, label)
        };
        if r.clicked() {
            match add_from_wizard(app, w, list) {
                Ok(name) => {
                    app.say(format!("{name} agregado"), RUN);
                    crate::connect::google_reset();
                    added = true;
                }
                Err(e) => w.err = e,
            }
        }
        if !ready {
            ui.label(faint("Puedes conectarlo después desde su tarjeta."));
        }
    });
    added
}

fn expand_dir(d: &str) -> std::path::PathBuf {
    crate::util::expand(if d.trim().is_empty() {
        "~/.claude"
    } else {
        d.trim()
    })
}

fn step(ui: &mut egui::Ui, n: usize, title: &str, sub: &str) {
    ui.horizontal_top(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
        ui.painter().circle(
            r.center(),
            10.5,
            RAISED2,
            Stroke::new(1.0, MASTER.gamma_multiply(0.6)),
        );
        ui.painter().text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            n.to_string(),
            medium(11.5),
            MASTER,
        );
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).font(medium(13.5)).color(INK));
            ui.label(faint(sub));
        });
    });
    ui.add_space(4.0);
}

fn add_from_wizard(app: &mut App, w: &Wizard, list: &[Value]) -> Result<String, String> {
    let sp = spec(w.choice);
    let name = if w.name.trim().is_empty() {
        sp.title.to_string()
    } else {
        w.name.trim().to_string()
    };
    if is_key_choice(w.choice) && !w.key.trim().is_empty() {
        crate::connect::save_key(sp.key_env, &w.key)?;
    }
    let mut c = app.sup.cfg.get();
    let ids: Vec<String> = list
        .iter()
        .map(|b| b.s("id"))
        .chain(c.arr("brains").iter().map(|b| b.s("id")))
        .collect();
    let base = slug(&name).replace('-', "_");
    let base = if base.is_empty() {
        "cerebro".to_string()
    } else {
        base
    };
    let mut id = base.clone();
    let mut n = 2;
    while ids.contains(&id) {
        id = format!("{base}_{n}");
        n += 1;
    }
    let numv = |s: &str| {
        s.trim()
            .parse::<f64>()
            .ok()
            .map(|x| json!(x))
            .unwrap_or(Value::Null)
    };
    let mut b = json!({
        "id": id, "kind": sp.kind, "name": name, "role": if w.role.is_empty() { "worker" } else { w.role.as_str() }, "enabled": true,
        "ladder": w.ladder.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        "budget": {"tokens_day": numv(&w.tokens_day), "usd_day": numv(&w.usd_day), "quota_pct": if sp.kind == "claude_code" { json!(90) } else { Value::Null }},
    });
    match sp.kind {
        "claude_code" => {
            b["config_dir"] = json!(w.config_dir.trim());
            b["match"] = json!({});
        }
        "codex" => {
            b["max_parallel"] = json!(2);
            b["codex_home"] = json!(w.config_dir.trim());
        }
        "antigravity" => {
            b["max_parallel"] = json!(2);
            // Gemini 4 Neon (thinking) first when the account has it, Hydrogen for routine.
            if w.ladder.trim().is_empty() {
                b["ladder"] = json!(crate::connect::google_models());
            }
        }
        "ollama" => b["base_url"] = json!(sp.base),
        _ => {
            b["base_url"] = json!(sp.base);
            b["key_env"] = json!(sp.key_env);
            b["max_parallel"] = json!(if w.choice == Choice::GeminiApi { 2 } else { 4 });
            if w.choice == Choice::DeepSeek {
                b["balance"] = json!("deepseek");
            }
        }
    }
    if let Some(arr) = c.get_mut("brains").and_then(|v| v.as_array_mut()) {
        arr.push(b);
    }
    let r = app.sup.act(&json!({"a": "config", "config": c}));
    if r == "ok" {
        Ok(name)
    } else {
        Err(r)
    }
}
