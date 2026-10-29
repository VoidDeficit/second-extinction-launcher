//! Launcher window styled after the game's menus: hero art and logo from Steam's library
//! cache, black/gold palette, diagonal stripes, a glowing PLAY button.

use crate::core::{self, Settings};
use crate::{load_settings, save_settings};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, TextureHandle, TextureOptions, Vec2,
    pos2, vec2,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, thread};

const GOLD: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x17);
const GOLD_HI: Color32 = Color32::from_rgb(0xFF, 0xCC, 0x4D);
const GOLD_LO: Color32 = Color32::from_rgb(0xB8, 0x6E, 0x0F);
const CREAM: Color32 = Color32::from_rgb(0xEC, 0xE3, 0xCB);
const MUTED: Color32 = Color32::from_rgb(0x9C, 0x93, 0x7E);
const BG: Color32 = Color32::from_rgb(0x09, 0x09, 0x0A);
const GREEN: Color32 = Color32::from_rgb(0x7C, 0xD0, 0x5C);
const RED: Color32 = Color32::from_rgb(0xE0, 0x55, 0x3D);

const SERVER_UNKNOWN: u8 = 0;
const SERVER_ONLINE: u8 = 1;
const SERVER_OFFLINE: u8 = 2;

enum Msg {
    Log(String),
    Done(Result<(), String>),
}

struct GameState {
    found: Result<PathBuf, String>,
    patched: bool,
    prefix: bool,
    running: bool,
}

struct App {
    settings: Settings,
    social_port: String,
    proxy_port: String,
    log: Vec<String>,
    show_log: bool,
    busy: bool,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    game: GameState,
    hero: Option<TextureHandle>,
    logo: Option<TextureHandle>,
    /// server address the probe thread checks, and its last result
    probe_target: Arc<Mutex<(String, u16)>>,
    server_state: Arc<AtomicU8>,
}

fn art_dir() -> Option<PathBuf> {
    let home = PathBuf::from(env::var_os("HOME")?);
    [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"]
        .iter()
        .map(|p| home.join(p).join("appcache/librarycache").join(core::APP_ID))
        .find(|p| p.is_dir())
}

fn load_texture(ctx: &egui::Context, name: &str) -> Option<TextureHandle> {
    let img = image::open(art_dir()?.join(name)).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture(name, color, TextureOptions::LINEAR))
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgba_unmultiplied(0, 0, 0, 170);
    v.override_text_color = Some(CREAM);
    v.selection.bg_fill = GOLD.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0_f32, GOLD_HI);
    v.text_cursor.stroke = Stroke::new(2.0_f32, GOLD_HI);
    v.hyperlink_color = GOLD_HI;
    for (w, stroke) in [
        (&mut v.widgets.inactive, Color32::from_white_alpha(28)),
        (&mut v.widgets.hovered, GOLD.gamma_multiply(0.8)),
        (&mut v.widgets.active, GOLD_HI),
    ] {
        w.bg_fill = Color32::from_rgba_unmultiplied(20, 20, 22, 200);
        w.weak_bg_fill = Color32::from_rgba_unmultiplied(20, 20, 22, 200);
        w.bg_stroke = Stroke::new(1.0_f32, stroke);
        w.fg_stroke = Stroke::new(1.0_f32, CREAM);
        w.corner_radius = CornerRadius::same(3);
    }
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, CREAM);
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = vec2(10.0, 10.0);
        s.spacing.button_padding = vec2(14.0, 7.0);
    });
}

/// Vertical gradient quad.
fn gradient(painter: &egui::Painter, r: Rect, top: Color32, bottom: Color32) {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.left_bottom(), bottom);
    m.colored_vertex(r.right_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 2, 3);
    painter.add(Shape::mesh(m));
}

/// Hero art scaled to cover `r`, cropped from the center.
fn cover_image(painter: &egui::Painter, tex: &TextureHandle, r: Rect, tint: Color32) {
    let [w, h] = tex.size().map(|x| x as f32);
    let scale = (r.width() / w).max(r.height() / h);
    let (uw, uh) = (r.width() / (w * scale), r.height() / (h * scale));
    let uv = Rect::from_min_max(pos2((1.0 - uw) / 2.0, (1.0 - uh) * 0.35), pos2((1.0 + uw) / 2.0, (1.0 - uh) * 0.35 + uh));
    painter.image(tex.id(), r, uv, tint);
}

/// Thin diagonal stripes like the game's menu backgrounds.
fn stripes(painter: &egui::Painter, r: Rect) {
    let stroke = Stroke::new(1.0_f32, Color32::from_white_alpha(5));
    let p = painter.with_clip_rect(r);
    let mut x = r.left() - r.height();
    while x < r.right() {
        p.line_segment([pos2(x, r.bottom()), pos2(x + r.height(), r.top())], stroke);
        x += 7.0;
    }
}

fn chip(ui: &mut egui::Ui, ok: Option<bool>, text: &str) {
    let color = match ok {
        Some(true) => GREEN,
        Some(false) => RED,
        None => MUTED,
    };
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 140))
        .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.45)))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.0, color);
                ui.label(egui::RichText::new(text).size(12.0).color(CREAM));
            });
        });
}

/// Big chamfered gold button with a hover glow. Returns true when clicked.
fn play_button(ui: &mut egui::Ui, size: Vec2, label: &str, enabled: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let hover = ui.ctx().animate_bool(resp.id, enabled && resp.hovered());
    let press = enabled && resp.is_pointer_button_down_on();
    let r = if press { rect.shrink(2.0) } else { rect };
    let p = ui.painter();

    if enabled {
        for i in 1..=6 {
            let a = ((7 - i) as f32 * (6.0 + 10.0 * hover)) as u8;
            p.rect_filled(r.expand(i as f32 * 3.0), CornerRadius::same(6 + i as u8 * 2), Color32::from_rgba_unmultiplied(0xE8, 0xA3, 0x17, a / 3));
        }
    }
    let c = 12.0; // chamfer like the game's buttons
    let pts = vec![
        pos2(r.left() + c, r.top()),
        pos2(r.right(), r.top()),
        pos2(r.right(), r.bottom() - c),
        pos2(r.right() - c, r.bottom()),
        pos2(r.left(), r.bottom()),
        pos2(r.left(), r.top() + c),
    ];
    let (top, bottom) = if enabled {
        (GOLD_HI.lerp_to_gamma(Color32::WHITE, 0.15 * hover), GOLD_LO.lerp_to_gamma(GOLD, 0.3 * hover))
    } else {
        (Color32::from_gray(70), Color32::from_gray(40))
    };
    // gradient fill clipped to the chamfered outline: fan of the polygon with per-vertex color
    let mut m = Mesh::default();
    for q in &pts {
        let t = (q.y - r.top()) / r.height();
        m.colored_vertex(*q, top.lerp_to_gamma(bottom, t));
    }
    for i in 1..pts.len() as u32 - 1 {
        m.add_triangle(0, i, i + 1);
    }
    p.add(Shape::mesh(m));
    p.add(Shape::closed_line(pts, Stroke::new(1.5_f32, if enabled { GOLD_HI } else { Color32::from_gray(90) })));
    // sheen on the upper half
    let sheen = Rect::from_min_max(r.left_top() + vec2(c, 2.0), pos2(r.right() - 2.0, r.center().y));
    gradient(p, sheen, Color32::from_white_alpha(40), Color32::TRANSPARENT);
    p.text(
        r.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(28.0),
        if enabled { Color32::from_rgb(0x1A, 0x10, 0x02) } else { Color32::from_gray(150) },
    );
    if enabled && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked() && enabled
}

fn ghost_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let b = egui::Button::new(egui::RichText::new(text).size(13.0).color(if enabled { CREAM } else { MUTED }))
        .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 120))
        .stroke(Stroke::new(1.0_f32, Color32::from_white_alpha(35)));
    ui.add_enabled(enabled, b)
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        let settings = load_settings();
        let (tx, rx) = channel();
        let probe_target = Arc::new(Mutex::new((settings.server.clone(), settings.social_port)));
        let server_state = Arc::new(AtomicU8::new(SERVER_UNKNOWN));
        {
            let (target, state, ctx) = (probe_target.clone(), server_state.clone(), cc.egui_ctx.clone());
            thread::spawn(move || {
                loop {
                    let (host, port) = target.lock().unwrap().clone();
                    let up = !host.trim().is_empty()
                        && ureq::get(&format!("http://{}:{port}/ca.pem", host.trim()))
                            .timeout(Duration::from_secs(2))
                            .call()
                            .is_ok();
                    let new = if up { SERVER_ONLINE } else { SERVER_OFFLINE };
                    if state.swap(new, Ordering::Relaxed) != new {
                        ctx.request_repaint();
                    }
                    thread::sleep(Duration::from_secs(3));
                }
            });
        }
        let mut app = App {
            social_port: settings.social_port.to_string(),
            proxy_port: settings.proxy_port.to_string(),
            settings,
            log: Vec::new(),
            show_log: true,
            busy: false,
            rx,
            tx,
            game: GameState { found: Err(String::new()), patched: false, prefix: false, running: false },
            hero: load_texture(&cc.egui_ctx, "library_hero.jpg"),
            logo: load_texture(&cc.egui_ctx, "logo.png"),
            probe_target,
            server_state,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        self.game = match core::find_game(&self.settings) {
            Ok(g) => {
                let st = core::status(&g);
                GameState { found: Ok(g.dir), patched: st.patched, prefix: st.ca_thumbprint.is_some(), running: core::game_running() }
            }
            Err(e) => GameState { found: Err(e), patched: false, prefix: false, running: false },
        };
    }

    fn apply_form(&mut self) -> Option<Settings> {
        let (Ok(sp), Ok(pp)) = (self.social_port.trim().parse(), self.proxy_port.trim().parse()) else {
            self.log.push("Error: ports must be numbers between 1 and 65535".into());
            self.show_log = true;
            return None;
        };
        self.settings.server = self.settings.server.trim().to_string();
        if self.settings.server.is_empty() {
            self.log.push("Error: enter the server address".into());
            self.show_log = true;
            return None;
        }
        self.settings.social_port = sp;
        self.settings.proxy_port = pp;
        save_settings(&self.settings);
        Some(self.settings.clone())
    }

    fn run(&mut self, job: fn(&Settings, &dyn Fn(String)) -> core::Result<()>, ctx: &egui::Context) {
        let Some(s) = self.apply_form() else { return };
        self.busy = true;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        thread::spawn(move || {
            let log = |m: String| {
                tx.send(Msg::Log(m)).ok();
                ctx.request_repaint();
            };
            let res = job(&s, &log);
            tx.send(Msg::Done(res)).ok();
            ctx.request_repaint();
        });
    }

    fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Log(l) => self.log.push(l),
                Msg::Done(r) => {
                    self.busy = false;
                    match r {
                        Ok(()) => self.log.push("Done — have fun, survivor.".into()),
                        Err(e) => {
                            self.log.push(format!("Error: {e}"));
                            self.show_log = true;
                        }
                    }
                    self.refresh();
                }
            }
        }
    }

    fn background(&self, ui: &egui::Ui, full: Rect) {
        let p = ui.painter();
        p.rect_filled(full, CornerRadius::ZERO, BG);
        if let Some(hero) = &self.hero {
            let band = Rect::from_min_size(full.min, vec2(full.width(), full.height() * 0.78));
            cover_image(p, hero, band, Color32::from_gray(200));
            gradient(p, band, Color32::from_black_alpha(40), BG);
        }
        // side vignette so text on the left stays readable
        let mut m = Mesh::default();
        for (q, a) in [(full.left_top(), 200), (full.left_bottom(), 230), (pos2(full.center().x, full.top()), 0), (pos2(full.center().x, full.bottom()), 60)] {
            m.colored_vertex(q, Color32::from_black_alpha(a));
        }
        m.add_triangle(0, 1, 2);
        m.add_triangle(1, 2, 3);
        p.add(Shape::mesh(m));
        stripes(p, full);
        // thin gold rule above the controls, like the in-game separators
        let y = full.bottom() - 212.0;
        let rule = Rect::from_min_max(pos2(full.left() + 40.0, y), pos2(full.right() - 40.0, y + 1.0));
        let mut m = Mesh::default();
        for (q, c) in [
            (rule.left_top(), Color32::TRANSPARENT),
            (rule.left_bottom(), Color32::TRANSPARENT),
            (rule.center_top(), GOLD.gamma_multiply(0.7)),
            (rule.center_bottom(), GOLD.gamma_multiply(0.7)),
            (rule.right_top(), Color32::TRANSPARENT),
            (rule.right_bottom(), Color32::TRANSPARENT),
        ] {
            m.colored_vertex(q, c);
        }
        for t in [[0, 1, 2], [1, 2, 3], [2, 3, 4], [3, 4, 5]] {
            m.add_triangle(t[0], t[1], t[2]);
        }
        p.add(Shape::mesh(m));
    }

    fn header(&self, ui: &egui::Ui, full: Rect) {
        let p = ui.painter();
        if let Some(logo) = &self.logo {
            let [w, h] = logo.size().map(|x| x as f32);
            let width = 380.0_f32.min(full.width() * 0.45);
            let r = Rect::from_min_size(full.min + vec2(28.0, 6.0), vec2(width, width * h / w));
            p.image(logo.id(), r, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
        } else {
            p.text(full.min + vec2(40.0, 40.0), Align2::LEFT_TOP, "SECOND", FontId::proportional(44.0), CREAM);
            p.text(full.min + vec2(40.0, 88.0), Align2::LEFT_TOP, "EXTINCTION", FontId::proportional(60.0), GOLD);
        }
        let tag = Rect::from_min_size(pos2(full.right() - 200.0, full.top() + 22.0), vec2(170.0, 26.0));
        p.rect_filled(tag, CornerRadius::same(13), Color32::from_black_alpha(150));
        p.rect_stroke(tag, CornerRadius::same(13), Stroke::new(1.0_f32, GOLD.gamma_multiply(0.6)), egui::StrokeKind::Inside);
        p.text(tag.center(), Align2::CENTER_CENTER, "CUSTOM SERVER", FontId::proportional(13.0), GOLD_HI);
    }

    fn controls(&mut self, ui: &mut egui::Ui, full: Rect, ctx: &egui::Context) {
        let area = Rect::from_min_max(pos2(full.left() + 40.0, full.bottom() - 196.0), pos2(full.right() - 40.0, full.bottom() - 24.0));
        let card_w = (area.width() - 320.0).max(360.0);
        let card = Rect::from_min_size(area.min, vec2(card_w, area.height()));

        ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(10, 10, 12, 205))
                .stroke(Stroke::new(1.0_f32, GOLD.gamma_multiply(0.25)))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.set_min_size(card.size() - vec2(32.0, 32.0));
                    ui.label(egui::RichText::new("SERVER").size(12.0).color(GOLD).strong());
                    ui.horizontal(|ui| {
                        let server = ui.add_sized(
                            [card_w - 230.0, 30.0],
                            egui::TextEdit::singleline(&mut self.settings.server).hint_text("IP or hostname").font(FontId::proportional(17.0)),
                        );
                        ui.add_sized([62.0, 30.0], egui::TextEdit::singleline(&mut self.social_port).font(FontId::proportional(15.0)))
                            .on_hover_text("Apex Connect port");
                        ui.add_sized([62.0, 30.0], egui::TextEdit::singleline(&mut self.proxy_port).font(FontId::proportional(15.0)))
                            .on_hover_text("Proxy port");
                        if server.changed() || server.lost_focus() {
                            *self.probe_target.lock().unwrap() =
                                (self.settings.server.clone(), self.social_port.trim().parse().unwrap_or(self.settings.social_port));
                        }
                    });
                    ui.add_space(2.0);
                    ui.horizontal_wrapped(|ui| {
                        let online = match self.server_state.load(Ordering::Relaxed) {
                            SERVER_ONLINE => Some(true),
                            SERVER_OFFLINE => Some(false),
                            _ => None,
                        };
                        chip(ui, online, match online {
                            Some(true) => "Server online",
                            Some(false) => "Server offline",
                            None => "Checking server…",
                        });
                        match &self.game.found {
                            Ok(_) => {
                                chip(ui, Some(self.game.patched), if self.game.patched { "Exe patched" } else { "Exe original" });
                                chip(ui, Some(self.game.prefix), if self.game.prefix { "Prefix ready" } else { "Prefix not set up" });
                                if self.game.running {
                                    chip(ui, None, "Game running");
                                }
                            }
                            Err(_) => chip(ui, Some(false), "Game not found"),
                        }
                    });
                    if let Err(e) = &self.game.found {
                        if !e.is_empty() {
                            ui.label(egui::RichText::new(e).size(12.0).color(RED));
                        }
                    }
                });
        });

        let side = Rect::from_min_max(pos2(card.right() + 24.0, area.top()), area.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(side), |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                let label = if self.busy { "LAUNCHING…" } else if self.game.running { "RUNNING" } else { "PLAY" };
                let can_play = !self.busy && !self.game.running && self.game.found.is_ok();
                if play_button(ui, vec2(side.width().min(280.0), 70.0), label, can_play) {
                    self.run(core::play, ctx);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let w = side.width().min(280.0);
                    ui.add_space((side.width() - w) / 2.0);
                    if ghost_button(ui, "↺  Revert", !self.busy && !self.game.running)
                        .on_hover_text("Restore the original exe and remove the server CA and proxy from the prefix")
                        .clicked()
                    {
                        self.run(core::revert, ctx);
                    }
                    if ghost_button(ui, "⟳  Refresh", !self.busy).clicked() {
                        self.refresh();
                    }
                    let log = if self.show_log { "▾ Log" } else { "▸ Log" };
                    if ghost_button(ui, log, true).clicked() {
                        self.show_log = !self.show_log;
                    }
                    if self.busy {
                        ui.add(egui::Spinner::new().color(GOLD_HI));
                    }
                });
            });
        });
    }

    /// Log panel docked along the bottom of the window.
    fn log_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("log")
            .exact_height(150.0)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(5, 5, 6))
                    .stroke(Stroke::new(1.0_f32, GOLD.gamma_multiply(0.35)))
                    .inner_margin(egui::Margin::symmetric(40, 10)),
            )
            .show_animated(ctx, self.show_log, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("LOG").size(12.0).color(GOLD).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.log.clear();
                        }
                    });
                });
                egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false; 2]).show(ui, |ui| {
                    if self.log.is_empty() {
                        ui.label(egui::RichText::new("Nothing yet. Press PLAY.").color(MUTED).monospace().size(12.0));
                    }
                    for l in &self.log {
                        let c = if l.starts_with("Error") { RED } else { CREAM };
                        ui.label(egui::RichText::new(l).monospace().size(12.0).color(c));
                    }
                });
            });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.drain();
        self.log_panel(ctx);
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
            let full = ui.max_rect();
            self.background(ui, full);
            self.header(ui, full);
            self.controls(ui, full, ctx);
        });
        // the game can start/exit outside the launcher; keep the chips current
        ctx.request_repaint_after(Duration::from_secs(2));
        if ctx.input(|i| i.time) as u64 % 2 == 0 {
            self.game.running = core::game_running();
        }
    }
}

pub fn run() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 740.0])
            .with_min_inner_size([820.0, 600.0])
            .with_title("Second Extinction — Custom Server"),
        ..Default::default()
    };
    eframe::run_native("se-launcher", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
