//! Second Extinction launcher for a custom backend (Linux / Proton).
//!
//! Without arguments it opens a window; with arguments it runs as a CLI:
//!   se-launcher [--data-dir DIR] [--instance-id ID] [--standalone] [--dev-mode]
//!   se-launcher --server HOST [--social-port N] [--proxy-port N] [--party-port N] [--game-dir DIR] [--no-launch]
//!   se-launcher --revert [--game-dir DIR]

mod core;
mod ui;

use core::Settings;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use std::{env, fs, process::exit};

/// Launcher state location. Resolved once from the global CLI options so that
/// `load_settings`/`save_settings` (also used by the UI) always see the same
/// per-instance directory.
struct Instance {
    id: Option<String>,
    dir: PathBuf,
}

static INSTANCE: OnceLock<Instance> = OnceLock::new();

/// Default state root: `$XDG_CONFIG_HOME/se-launcher` (or `~/.config/se-launcher`).
#[cfg(windows)]
fn default_data_root() -> Option<PathBuf> {
    Some(PathBuf::from(env::var_os("APPDATA")?).join("se-launcher"))
}

#[cfg(not(windows))]
fn default_data_root() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("se-launcher"))
}

/// `--data-dir` overrides the state root; `--instance-id` adds an isolated
/// profile below it. Without an instance the layout is unchanged.
fn init_instance(data_dir: Option<PathBuf>, id: Option<String>) -> Result<(), String> {
    let root = data_dir
        .or_else(default_data_root)
        .ok_or("cannot determine the config directory; pass --data-dir")?;
    let dir = match &id {
        Some(id) => root.join("instances").join(id),
        None => root,
    };
    INSTANCE.set(Instance { id, dir }).map_err(|_| "instance already initialized".to_string())
}

pub fn instance_id() -> Option<&'static str> {
    INSTANCE.get().and_then(|i| i.id.as_deref())
}

pub fn instance_dir() -> Option<&'static Path> {
    INSTANCE.get().map(|i| i.dir.as_path())
}

fn config_path() -> Option<PathBuf> {
    Some(INSTANCE.get()?.dir.join("settings"))
}

pub fn load_settings() -> Settings {
    let mut s = Settings::default();
    let text = config_path().and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
    for line in text.lines() {
        match line.split_once('=') {
            Some(("server", v)) => s.server = v.to_string(),
            Some(("social_port", v)) => s.social_port = v.parse().unwrap_or(s.social_port),
            Some(("proxy_port", v)) => s.proxy_port = v.parse().unwrap_or(s.proxy_port),
            Some(("party_port", v)) => s.party_port = v.parse().unwrap_or(s.party_port),
            Some(("game_dir", v)) if !v.is_empty() => s.game_dir = Some(PathBuf::from(v)),
            Some(("standalone", v)) => s.standalone = matches!(v, "1" | "true" | "yes"),
            Some(("dev_mode", v)) => s.dev_mode = matches!(v, "1" | "true" | "yes"),
            Some(("mixed", v)) => s.mixed = matches!(v, "1" | "true" | "yes"),
            Some(("party", v)) => s.party = matches!(v, "1" | "true" | "yes"),
            Some(("skip_intro", v)) => s.skip_intro = matches!(v, "1" | "true" | "yes"),
            Some(("emulator_dir", v)) if !v.is_empty() => s.emulator_dir = Some(PathBuf::from(v)),
            _ => {}
        }
    }
    s
}

pub fn save_settings(s: &Settings) {
    let Some(p) = config_path() else { return };
    fs::create_dir_all(p.parent().unwrap()).ok();
    let dir = s.game_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
    let emulator = s.emulator_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
    fs::write(p, format!(
        "server={}\nsocial_port={}\nproxy_port={}\nparty_port={}\ngame_dir={dir}\nstandalone={}\ndev_mode={}\nmixed={}\nparty={}\nskip_intro={}\nemulator_dir={emulator}\n",
        s.server,
        s.social_port,
        s.proxy_port,
        s.party_port,
        u8::from(s.standalone),
        u8::from(s.dev_mode),
        u8::from(s.mixed),
        u8::from(s.party),
        u8::from(s.skip_intro)
    ))
    .ok();
}

// ---- CLI ------------------------------------------------------------------------

fn usage() -> ! {
    eprintln!(
        "usage: se-launcher [--data-dir DIR] [--instance-id ID] [--standalone]\n\
         \x20                  (opens the launcher window)\n\
         \x20      se-launcher [--data-dir DIR] [--instance-id ID] --server HOST\n\
         \x20                  [--social-port 8443] [--proxy-port 8888] [--party-port 8450]\n\
         \x20                  [--game-dir DIR] [--emulator-dir DIR] [--no-launch]\n\
         \x20                  [--standalone] [--dev-mode] [--mixed] [--party]\n\
         \x20      se-launcher [--data-dir DIR] [--instance-id ID] --revert [--game-dir DIR]\n\
         \n\
         \x20 --data-dir DIR    state directory (default: $XDG_CONFIG_HOME/se-launcher)\n\
         \x20 --instance-id ID  isolated settings profile below the state directory\n\
         \x20 --standalone      launch through Proton directly instead of Steam\n\
         \x20                   (alias: --offline; needs the Steam emulator)\n\
         \x20 --emulator-dir DIR  Goldberg release used for non-Steam play\n\
         \x20 --dev-mode        allow multiple clients, each with its own player identity\n\
         \x20                   (implies --standalone)\n\
         \x20 --mixed           start one real Steam client plus one Goldberg client\n\
         \x20 --party           keep PlayFab Party crossplay (do not patch the exe)"
    );
    exit(2)
}

/// Options that apply to both the window and the CLI, and may appear anywhere.
#[derive(Default, Clone)]
pub struct Overrides {
    pub standalone: Option<bool>,
    pub dev_mode: Option<bool>,
    pub emulator_dir: Option<PathBuf>,
}

#[derive(Default)]
struct Globals {
    data_dir: Option<PathBuf>,
    instance_id: Option<String>,
    overrides: Overrides,
}

fn parse_globals(args: Vec<String>) -> Result<(Globals, Vec<String>), String> {
    let mut g = Globals::default();
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--data-dir" => {
                g.data_dir = Some(PathBuf::from(it.next().ok_or("--data-dir needs a directory")?))
            }
            "--instance-id" => {
                let id = it.next().ok_or("--instance-id needs a name")?;
                if id.is_empty()
                    || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                {
                    return Err(format!("invalid instance id {id:?}; use letters, digits, '.', '_' or '-'"));
                }
                g.instance_id = Some(id);
            }
            "--standalone" | "--offline" => g.overrides.standalone = Some(true),
            "--dev-mode" => g.overrides.dev_mode = Some(true),
            "--emulator-dir" => {
                g.overrides.emulator_dir = Some(PathBuf::from(it.next().ok_or("--emulator-dir needs a directory")?))
            }
            _ => rest.push(a),
        }
    }
    Ok((g, rest))
}

fn run_cli(args: Vec<String>, overrides: Overrides) {
    let mut s = Settings {
        standalone: overrides.standalone.unwrap_or(false),
        dev_mode: overrides.dev_mode.unwrap_or(false),
        emulator_dir: overrides.emulator_dir,
        ..Settings::default()
    };
    let (mut revert, mut no_launch, mut server) = (false, false, None);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--server" => server = Some(val()),
            "--social-port" => s.social_port = val().parse().unwrap_or_else(|_| usage()),
            "--proxy-port" => s.proxy_port = val().parse().unwrap_or_else(|_| usage()),
            "--party-port" => s.party_port = val().parse().unwrap_or_else(|_| usage()),
            "--game-dir" => s.game_dir = Some(PathBuf::from(val())),
            "--mixed" => s.mixed = true,
            "--party" => s.party = true,
            "--revert" => revert = true,
            "--no-launch" => no_launch = true,
            _ => usage(),
        }
    }
    if s.dev_mode {
        s.standalone = true;
    }
    let log = |m: String| println!("{m}");
    let res = if revert {
        core::revert(&s, &log)
    } else {
        s.server = server.unwrap_or_else(|| usage());
        if no_launch {
            core::find_game(&s).and_then(|g| {
                core::patch_exe(&g, &log)?;
                if s.standalone {
                    core::install_emulator(&g, &s, &log)?;
                    if !s.dev_mode {
                        core::prepare_standalone_identity(&g, core::local_steam_account().as_ref(), &log)?;
                    }
                }
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
    let (globals, args) = parse_globals(args).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        usage()
    });
    if let Err(e) = init_instance(globals.data_dir, globals.instance_id) {
        eprintln!("error: {e}");
        exit(1);
    }
    if !args.is_empty() {
        return run_cli(args, globals.overrides);
    }
    if let Err(e) = ui::run(globals.overrides) {
        eprintln!("error: cannot open window: {e}");
        exit(1);
    }
}
