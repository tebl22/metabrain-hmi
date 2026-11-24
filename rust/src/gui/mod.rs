//! MetaBrain's native window (egui/eframe): the whole HMI in Rust, no browser.
//! The supervisor runs in-process; a poller thread keeps a fresh snapshot so the UI never blocks.

pub mod home;
pub mod pages;
mod response;
mod shell;
pub mod theme;

use crate::core::Supervisor;
use crate::util::{now, J};
use eframe::egui::{self, Color32, CornerRadius, Margin, Stroke, Vec2};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use theme::*;

/// Set by a second MetaBrain.exe (through POST /api/show) or the tray: bring the window up.
static SHOW: AtomicBool = AtomicBool::new(false);
static CTX: OnceLock<egui::Context> = OnceLock::new();

/// The main window's handle (Windows), so another thread can bring it back even while
/// the window is hidden and egui is not drawing frames.
#[cfg_attr(not(windows), allow(dead_code))]
static HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

pub fn request_show() {
    SHOW.store(true, Ordering::SeqCst);
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
        };
        let h = HWND.load(Ordering::SeqCst);
        if h != 0 {
            // SAFETY: plain Win32 calls on our own top-level window handle.
            unsafe {
                ShowWindow(h as _, SW_SHOW);
                ShowWindow(h as _, SW_RESTORE);
                SetForegroundWindow(h as _);
            }
        }
    }
    if let Some(c) = CTX.get() {
        c.request_repaint();
    }
}

#[derive(Default)]
pub struct Data {
    pub snap: Value,
    pub approvals: Value,
    pub tasks: Value,
    pub parallel_runs: Vec<crate::parallel::ParallelRun>,
    pub savings: Value,
    pub router: Value,
    pub doctor: Value,
    pub arch: Value,
    pub at: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Home,
    Tasks,
    Brains,
    Usage,
    Alarms,
    Router,
    Memory,
    Services,
    Agents,
    Extensions,
    Settings,
}

impl Page {
    const ALL: [Page; 11] = [
        Page::Home,
        Page::Tasks,
        Page::Brains,
        Page::Usage,
        Page::Alarms,
        Page::Router,
        Page::Memory,
        Page::Services,
        Page::Agents,
        Page::Extensions,
        Page::Settings,
    ];
    fn label(self) -> &'static str {
        match self {
            Page::Home => "Home",
            Page::Tasks => "Chat",
            Page::Brains => "Brains",
            Page::Usage => "Usage & Savings",
            Page::Alarms => "Alarms",
            Page::Router => "Router",
            Page::Memory => "Memory",
            Page::Services => "Services",
            Page::Agents => "Agents",
            Page::Extensions => "Extensions",
            Page::Settings => "Settings",
        }
    }
    #[allow(dead_code)]
    fn icon(self) -> Icon {
        match self {
            Page::Home => Icon::Home,
            Page::Tasks => Icon::Tasks,
            Page::Brains => Icon::Brains,
            Page::Usage => Icon::Usage,
            Page::Alarms => Icon::Alarms,
            Page::Router => Icon::Router,
            Page::Memory => Icon::Memory,
            Page::Services => Icon::Services,
            Page::Agents => Icon::Agents,
            Page::Extensions => Icon::Extensions,
            Page::Settings => Icon::Settings,
        }
    }
}

/// A short message at the bottom of the window ("Guardado", errors).
pub struct Toast {
    pub text: String,
    pub color: Color32,
    pub until: f64,
}

pub struct App {
    pub sup: Arc<Supervisor>,
    pub port: u16,
    /// Responses of the local API (path -> (fetched at, body)); filled by background threads.
    pub cache: Arc<RwLock<std::collections::HashMap<String, (f64, Value)>>>,
    /// Messages from background work, shown as toasts.
    pub inbox: Arc<std::sync::Mutex<Vec<(String, Color32)>>>,
    pub data: Arc<RwLock<Data>>,
    pub page: Page,
    pub toast: Option<Toast>,
    pub ui: pages::UiState,
    quitting: bool,
    #[cfg(windows)]
    _tray: Option<tray_icon::TrayIcon>,
}

impl App {
    /// Run a supervisor action off the UI thread, then refresh. Surfaces non-"ok" results as a toast.
    pub fn act(&self, d: Value) {
        let sup = self.sup.clone();
        let data = self.data.clone();
        let inbox = self.inbox.clone();
        std::thread::spawn(move || {
            let res = sup.act(&d);
            if !res.is_empty() && res != "ok" {
                let col = if res == "confirm" { ATTN } else { ALARM };
                if let Ok(mut i) = inbox.lock() {
                    i.push((res, col));
                }
            }
            refresh(&sup, &data, true);
        });
    }

    /// Cached GET against the in-process API; refetches in the background when older than `max_age` s.
    pub fn get(&self, path: &str, max_age: f64) -> Value {
        let hit = self.cache.read().ok().and_then(|c| c.get(path).cloned());
        let stale = hit
            .as_ref()
            .map(|(t, _)| now() - t > max_age)
            .unwrap_or(true);
        if stale {
            let fresh_marker = (
                now(),
                hit.as_ref().map(|h| h.1.clone()).unwrap_or(Value::Null),
            );
            if let Ok(mut c) = self.cache.write() {
                c.insert(path.to_string(), fresh_marker); // one fetch in flight at a time
            }
            let (cache, url, key) = (
                self.cache.clone(),
                format!(
                    "http://127.0.0.1:{}/{}",
                    self.port,
                    path.trim_start_matches('/')
                ),
                path.to_string(),
            );
            std::thread::spawn(move || {
                if let Ok(v) = crate::util::http_json(&url, &[], None, 30) {
                    if let Ok(mut c) = cache.write() {
                        c.insert(key, (now(), v));
                    }
                    if let Some(x) = CTX.get() {
                        x.request_repaint();
                    }
                }
            });
        }
        hit.map(|h| h.1).unwrap_or(Value::Null)
    }

    pub fn forget(&self, path: &str) {
        if let Ok(mut c) = self.cache.write() {
            c.remove(path);
        }
    }

    /// POST to the in-process API in the background; `done` turns the answer into a toast.
    pub fn post(
        &self,
        path: &str,
        body: Value,
        done: impl FnOnce(&Value) -> (String, Color32) + Send + 'static,
    ) {
        let (url, key, cache, inbox, sup, data) = (
            format!(
                "http://127.0.0.1:{}/{}",
                self.port,
                path.trim_start_matches('/')
            ),
            path.to_string(),
            self.cache.clone(),
            self.inbox.clone(),
            self.sup.clone(),
            self.data.clone(),
        );
        std::thread::spawn(move || {
            let r = crate::util::local_post_json(&url, &body, 120)
                .unwrap_or_else(|e| json!({"error": e}));
            // Invalidate any cached GET for this endpoint AFTER the POST finishes so the
            // next frame never refetches stale pre-mutation state.
            if let Ok(mut c) = cache.write() {
                c.remove(&key);
            }
            let msg = done(&r);
            if !msg.0.is_empty() {
                if let Ok(mut i) = inbox.lock() {
                    i.push(msg);
                }
            }
            if let Some(ctx) = CTX.get() {
                ctx.request_repaint();
            }
            refresh(&sup, &data, false);
        });
    }

    pub fn say(&mut self, text: impl Into<String>, color: Color32) {
        self.toast = Some(Toast {
            text: text.into(),
            color,
            until: now() + 3.5,
        });
    }
}

/// Fill the shared data. `full` also refreshes the slow sources (savings, router, doctor).
fn refresh(sup: &Arc<Supervisor>, data: &Arc<RwLock<Data>>, full: bool) {
    let snap = sup.snapshot();
    let (_, wait_s) = crate::approvals::settings(&sup.cfg.get());
    let approvals = crate::approvals::list(30, wait_s);
    let tasks = sup.tasks.list_with_queue(200);
    let parallel_runs = sup.parallel.list_cached(40);
    let arch = sup.architecture();
    let (savings, router, doctor) = if full {
        (
            Some(crate::savings::report(&sup.cfg.brains(), 14)),
            Some(sup.router_table(None)),
            Some(crate::insights::doctor(&sup.cfg.get())),
        )
    } else {
        (None, None, None)
    };
    let snap = if std::env::var("METABRAIN_DEMO").is_ok() {
        demo(snap)
    } else {
        snap
    };
    if let Ok(mut d) = data.write() {
        d.snap = snap;
        d.approvals = approvals;
        d.tasks = tasks;
        d.parallel_runs = parallel_runs;
        d.arch = arch;
        if let Some(s) = savings {
            d.savings = s;
        }
        if let Some(r) = router {
            d.router = r;
        }
        if let Some(x) = doctor {
            d.doctor = x;
        }
        d.at = now();
    }
    if let Some(c) = CTX.get() {
        c.request_repaint();
    }
}

/// Open the native window. `hidden` starts in the tray (login autostart).
pub fn run(sup: Arc<Supervisor>, port: u16, hidden: bool) -> Result<(), String> {
    let data = Arc::new(RwLock::new(Data::default()));
    {
        let (s, d) = (sup.clone(), data.clone());
        std::thread::spawn(move || {
            let mut n = 0u64;
            loop {
                refresh(&s, &d, n.is_multiple_of(30));
                n += 1;
                std::thread::sleep(Duration::from_millis(1000));
            }
        });
    }

    let icon = crate::app::tray_rgba().map(|(rgba, w, h)| {
        Arc::new(egui::IconData {
            rgba,
            width: w,
            height: h,
        })
    });
    let mut vp = egui::ViewportBuilder::default()
        .with_title("MetaBrain")
        .with_app_id("MetaBrain")
        .with_inner_size([1120.0, 700.0])
        .with_min_inner_size([640.0, 480.0])
        .with_visible(!hidden);
    if let Some(i) = icon {
        vp = vp.with_icon(i);
    }
    let opts = eframe::NativeOptions {
        viewport: vp,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "MetaBrain",
        opts,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let _ = CTX.set(cc.egui_ctx.clone());
            #[cfg(windows)]
            {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                if let Ok(wh) = cc.window_handle() {
                    if let RawWindowHandle::Win32(w) = wh.as_raw() {
                        HWND.store(w.hwnd.get(), Ordering::SeqCst);
                    }
                }
            }
            #[cfg(windows)]
            let tray = tray::build(&cc.egui_ctx);
            Ok(Box::new(App {
                sup,
                port,
                cache: Default::default(),
                inbox: Default::default(),
                data,
                page: std::env::var("METABRAIN_PAGE")
                    .ok()
                    .and_then(|p| {
                        Page::ALL.iter().copied().find(|x| {
                            x.label().eq_ignore_ascii_case(&p)
                                || format!("{x:?}").eq_ignore_ascii_case(&p)
                        })
                    })
                    .unwrap_or(Page::Home),
                toast: None,
                ui: pages::UiState::default(),
                quitting: false,
                #[cfg(windows)]
                _tray: tray,
            }))
        }),
    )
    .map_err(|e| e.to_string())
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(windows)]
        if let Some(cmd) = tray::poll() {
            match cmd {
                tray::Cmd::Open => request_show(),
                tray::Cmd::PauseAll => self.act(json!({"a": "pause_all"})),
                tray::Cmd::ResumeAll => self.act(json!({"a": "resume_all"})),
                tray::Cmd::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if SHOW.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        // Closing the window keeps MetaBrain supervising from the tray.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && cfg!(windows) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        let msgs: Vec<(String, Color32)> = self
            .inbox
            .lock()
            .map(|mut i| i.drain(..).collect())
            .unwrap_or_default();
        if let Some((t, c)) = msgs.into_iter().last() {
            self.say(t, c);
        }
        ctx.request_repaint_after(Duration::from_millis(1000));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self
            .data
            .read()
            .map(|d| d.snap.clone())
            .unwrap_or(Value::Null);
        let pending = self
            .data
            .read()
            .map(|d| d.approvals.arr("pending").len())
            .unwrap_or(0);

        shell::sidebar(self, ui, &snap, pending);

        let pad_x = if ui.ctx().content_rect().width() < 760.0 {
            14
        } else {
            26
        };
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(FIELD).inner_margin(Margin {
                left: pad_x,
                right: pad_x,
                top: 16,
                bottom: 14,
            }))
            .show(ui, |ui| {
                if snap.is_null() {
                    ui.centered_and_justified(|ui| ui.label(dim("Leyendo los cerebros…")));
                    return;
                }
                if self.page == Page::Tasks {
                    pages::show(self, ui, &snap, Page::Tasks);
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt(self.page as usize)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Keep clear of the scroll bar.
                        ui.set_max_width((ui.available_width() - 14.0).max(1.0));
                        match self.page {
                            Page::Home => home::show(self, ui, &snap),
                            p => pages::show(self, ui, &snap, p),
                        }
                    });
            });

        if !snap.is_null() {
            shell::bell(self, ui.ctx(), &snap);
            shell::palette(self, ui.ctx(), &snap);
        }
        pages::approvals_overlay(self, ui.ctx());
        self.paint_toast(ui.ctx());
    }
}

impl App {
    fn paint_toast(&mut self, ctx: &egui::Context) {
        let Some(t) = &self.toast else { return };
        if now() > t.until {
            self.toast = None;
            return;
        }
        let (text, color) = (t.text.clone(), t.color);
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, Vec2::new(0.0, -18.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(RAISED)
                    .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
                    .corner_radius(CornerRadius::same(R))
                    .inner_margin(Margin::symmetric(14, 8))
                    .show(ui, |ui| ui.label(egui::RichText::new(text).color(INK)));
            });
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

// ---------- Windows tray ----------
#[cfg(windows)]
mod tray {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Mutex;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, TrayIconBuilder, TrayIconEvent};

    pub enum Cmd {
        Open,
        PauseAll,
        ResumeAll,
        Quit,
    }

    static RX: OnceLock<Mutex<Receiver<Cmd>>> = OnceLock::new();

    pub fn poll() -> Option<Cmd> {
        RX.get()?.lock().ok()?.try_recv().ok()
    }

    pub fn build(ctx: &egui::Context) -> Option<tray_icon::TrayIcon> {
        let (rgba, w, h) = crate::app::tray_rgba()?;
        let icon = tray_icon::Icon::from_rgba(rgba, w, h).ok()?;
        let open = MenuItem::new("Abrir MetaBrain", true, None);
        let pause = MenuItem::new("Pausar todo", true, None);
        let resume = MenuItem::new("Reanudar todo", true, None);
        let quit = MenuItem::new("Salir de MetaBrain", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &pause,
            &resume,
            &PredefinedMenuItem::separator(),
            &quit,
        ]);
        let ids = (
            open.id().clone(),
            pause.id().clone(),
            resume.id().clone(),
            quit.id().clone(),
        );

        let (tx, rx) = channel::<Cmd>();
        let _ = RX.set(Mutex::new(rx));
        let tx_menu = tx.clone();
        let c1 = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let cmd = if e.id == ids.0 {
                Cmd::Open
            } else if e.id == ids.1 {
                Cmd::PauseAll
            } else if e.id == ids.2 {
                Cmd::ResumeAll
            } else if e.id == ids.3 {
                Cmd::Quit
            } else {
                return;
            };
            if matches!(cmd, Cmd::Open) {
                super::request_show();
            }
            let _ = tx_menu.send(cmd);
            c1.request_repaint();
        }));
        let c2 = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = e
            {
                super::request_show();
                let _ = tx.send(Cmd::Open);
                c2.request_repaint();
            }
        }));
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("MetaBrain")
            .with_icon(icon)
            .build()
            .ok()
    }
}

/// Sample activity for screenshots of the design (METABRAIN_DEMO=1); never used otherwise.
fn demo(mut snap: Value) -> Value {
    let t = now();
    if let Some(bs) = snap.get_mut("brains").and_then(|v| v.as_array_mut()) {
        for (i, b) in bs.iter_mut().enumerate() {
            let subs: Vec<Value> = (0..[4, 2, 3, 1][i % 4])
                .map(|k: usize| json!({"id": format!("x{k}"), "desc": (["Revisar tests del parser", "Buscar usos de config", "Refactor de router.rs", "Leer documentación"][k % 4]), "model": "claude-haiku-5-5", "tokens": 12000 * (k + 1), "alive": k.is_multiple_of(2), "type": "general"}))
                .collect();
            b["subagents"] = json!(subs);
            b["active"] = json!(i < 2);
            b["quota"] = json!([{"group": "session", "pct": ([34.0, 71.0, 92.0, 12.0][i % 4]), "reset": t + 3600.0 * (i as f64 + 1.0)}]);
            b["usage"] = json!({"today": 180000.0 * (i as f64 + 1.0), "d7": 2_400_000.0 / (i as f64 + 1.0), "h5": 90000.0, "hourly": (0..24).map(|h| ((h * 7 + i * 13) % 11) as f64 * 1000.0).collect::<Vec<_>>(), "daily": (0..30).map(|d| ((d * 5 + i * 3) % 13) as f64 * 40000.0 + 20000.0).collect::<Vec<_>>()});
        }
        if bs.len() > 2 {
            let to = bs[2].s("id");
            bs[0]["jobs"] = json!([{"to": to, "task": "Escribir pruebas", "alive": true}]);
        }
    }
    snap
}
