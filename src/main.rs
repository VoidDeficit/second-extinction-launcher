//! Second Extinction launcher for a custom backend (Linux / Proton).
//!
//! Without arguments it opens a window; with arguments it runs as a CLI:
//!   se-launcher --server HOST [--social-port N] [--proxy-port N] [--game-dir DIR] [--no-launch]
//!   se-launcher --revert [--game-dir DIR]

mod core;

use core::Settings;
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::{env, fs, process::exit, thread};

fn config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("se-launcher/settings"))
}

fn load_settings() -> Settings {
    let mut s = Settings::default();
    let text = config_path().and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
    for line in text.lines() {
        match line.split_once('=') {
            Some(("server", v)) => s.server = v.to_string(),
            Some(("social_port", v)) => s.social_port = v.parse().unwrap_or(s.social_port),
            Some(("proxy_port", v)) => s.proxy_port = v.parse().unwrap_or(s.proxy_port),
            Some(("game_dir", v)) if !v.is_empty() => s.game_dir = Some(PathBuf::from(v)),
            _ => {}
        }
    }
    s
}

fn save_settings(s: &Settings) {
    let Some(p) = config_path() else { return };
    fs::create_dir_all(p.parent().unwrap()).ok();
    let dir = s.game_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
    fs::write(p, format!(
        "server={}\nsocial_port={}\nproxy_port={}\ngame_dir={dir}\n",
        s.server, s.social_port, s.proxy_port
    ))
    .ok();
}

// ---- CLI ------------------------------------------------------------------------

fn usage() -> ! {
    eprintln!(
        "usage: se-launcher                 (opens the launcher window)\n\
         \x20      se-launcher --server HOST [--social-port 8443] [--proxy-port 8888]\n\
         \x20                  [--game-dir DIR] [--no-launch]\n\
         \x20      se-launcher --revert [--game-dir DIR]"
    );
    exit(2)
}

fn run_cli(args: Vec<String>) {
    let mut s = Settings::default();
    let (mut revert, mut no_launch, mut server) = (false, false, None);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--server" => server = Some(val()),
            "--social-port" => s.social_port = val().parse().unwrap_or_else(|_| usage()),
            "--proxy-port" => s.proxy_port = val().parse().unwrap_or_else(|_| usage()),
            "--game-dir" => s.game_dir = Some(PathBuf::from(val())),
            "--revert" => revert = true,
            "--no-launch" => no_launch = true,
            _ => usage(),
        }
    }
    let log = |m: String| println!("{m}");
    let res = if revert {
        core::revert(&s, &log)
    } else {
        s.server = server.unwrap_or_else(|| usage());
        if no_launch {
            core::find_game(&s).and_then(|g| {
                core::patch_exe(&g, &log)?;
                core::setup_prefix(&g, &s, &log)?;
                log(format!("Launch skipped; game arguments: {}", core::launch_args(&s).join(" ")));
                Ok(())
            })
        } else {
            core::play(&s, &log)
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        exit(1);
    }
}

// ---- GUI ------------------------------------------------------------------------

enum Msg {
    Log(String),
    Done(Result<(), String>),
}

struct App {
    settings: Settings,
    social_port: String,
    proxy_port: String,
    log: Vec<String>,
    busy: bool,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    status: String,
}

impl App {
    fn new() -> Self {
        let settings = load_settings();
        let (tx, rx) = channel();
        let mut app = App {
            social_port: settings.social_port.to_string(),
            proxy_port: settings.proxy_port.to_string(),
            settings,
            log: Vec::new(),
            busy: false,
            rx,
            tx,
            status: String::new(),
        };
        app.refresh_status();
        app
    }

    fn refresh_status(&mut self) {
        self.status = match core::find_game(&self.settings) {
            Ok(g) => {
                let st = core::status(&g);
                format!(
                    "Game: {}\nExe: {}   Prefix: {}{}",
                    g.dir.display(),
                    if st.patched { "patched (Steam matchmaking)" } else { "original" },
                    match &st.ca_thumbprint {
                        Some(t) => format!("server CA {}…", &t[..t.len().min(12)]),
                        None => "not configured".into(),
                    },
                    if core::game_running() { "   ● running" } else { "" }
                )
            }
            Err(e) => format!("⚠ {e}"),
        };
    }

    /// Validate the form into settings; None (with a log line) if a port is invalid.
    fn apply_form(&mut self) -> Option<Settings> {
        let (Ok(sp), Ok(pp)) = (self.social_port.trim().parse(), self.proxy_port.trim().parse()) else {
            self.log.push("Ports must be numbers between 1 and 65535".into());
            return None;
        };
        self.settings.server = self.settings.server.trim().to_string();
        if self.settings.server.is_empty() {
            self.log.push("Enter the server address".into());
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
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Log(l) => self.log.push(l),
                Msg::Done(r) => {
                    self.busy = false;
                    match r {
                        Ok(()) => self.log.push("Done.".into()),
                        Err(e) => self.log.push(format!("Error: {e}")),
                    }
                    self.refresh_status();
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Second Extinction — custom server");
            ui.add_space(8.0);

            egui::Grid::new("form").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Server");
                ui.add(egui::TextEdit::singleline(&mut self.settings.server).hint_text("IP or hostname").desired_width(260.0));
                ui.end_row();
                ui.label("Social port");
                ui.add(egui::TextEdit::singleline(&mut self.social_port).desired_width(80.0));
                ui.end_row();
                ui.label("Proxy port");
                ui.add(egui::TextEdit::singleline(&mut self.proxy_port).desired_width(80.0));
                ui.end_row();
            });

            ui.add_space(8.0);
            ui.label(egui::RichText::new(&self.status).small());
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    let play = egui::Button::new(egui::RichText::new("▶  Play").size(18.0)).min_size([140.0, 36.0].into());
                    if ui.add(play).on_hover_text("Patch the exe, configure the Proton prefix and start the game").clicked() {
                        self.run(core::play, ctx);
                    }
                    if ui.button("Revert").on_hover_text("Restore the original exe and remove the server CA/proxy").clicked() {
                        self.run(core::revert, ctx);
                    }
                    if ui.button("Refresh").clicked() {
                        self.refresh_status();
                    }
                });
                if self.busy {
                    ui.spinner();
                }
            });

            ui.add_space(8.0);
            ui.separator();
            egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false; 2]).show(ui, |ui| {
                for l in &self.log {
                    let text = egui::RichText::new(l).monospace();
                    ui.label(if l.starts_with("Error") { text.color(ui.visuals().error_fg_color) } else { text });
                }
            });
        });
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if !args.is_empty() {
        return run_cli(args);
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([520.0, 420.0]).with_title("Second Extinction Launcher"),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("se-launcher", opts, Box::new(|_| Ok(Box::new(App::new())))) {
        eprintln!("error: cannot open window: {e}");
        exit(1);
    }
}
