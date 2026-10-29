//! Second Extinction launcher for a custom backend (Linux / Proton).
//!
//! Without arguments it opens a window; with arguments it runs as a CLI:
//!   se-launcher --server HOST [--social-port N] [--proxy-port N] [--game-dir DIR] [--no-launch]
//!   se-launcher --revert [--game-dir DIR]

mod core;
mod ui;

use core::Settings;

use std::path::PathBuf;

use std::{env, fs, process::exit};

fn config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("se-launcher/settings"))
}

pub fn load_settings() -> Settings {
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

pub fn save_settings(s: &Settings) {
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

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if !args.is_empty() {
        return run_cli(args);
    }
    if let Err(e) = ui::run() {
        eprintln!("error: cannot open window: {e}");
        exit(1);
    }
}
