//! Game discovery, exe patching, Proton prefix setup and launch.
//!
//! Steps: find the game, patch the exe (Steam matchmaking instead of PlayFab Party),
//! trust the server's CA and route WinHTTP through its proxy in the Proton prefix,
//! then start the game through Steam (or Proton directly in standalone mode) with the
//! Apex Connect redirect flags.

use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

pub type Result<T> = std::result::Result<T, String>;

pub const APP_ID: &str = "1024380";
const CROSSPLAY_OFFSET: usize = 0x34A4AA0;
const EXE: &str = "SecondExtinction_F.exe";
/// sha256 of the build the patch offsets were taken from.
const EXE_SHA256: &str = "1a80f8b3168be1ba24e5f1d47da959c5ddbe54e4e6cc76ea7f54e31bfc483743";
/// (file offset, original, patched): crossplay flag default (VA 0x1434a6aa0) 1 -> 0, which makes
/// the backend choice at 0x140bf34f5 build the Steam matchmaking backend instead of PlayFab Party.
const PATCHES: &[(usize, u8, u8)] = &[(0x34A4AA0, 1, 0)];

const CERT_KEY: &str = r"HKEY_LOCAL_MACHINE\Software\Microsoft\SystemCertificates\Root\Certificates";
const CONN_KEY: &str =
    r"HKEY_LOCAL_MACHINE\Software\Microsoft\Windows\CurrentVersion\Internet Settings\Connections";

#[derive(Clone)]
pub struct Settings {
    pub server: String,
    pub social_port: u16,
    pub proxy_port: u16,
    pub game_dir: Option<PathBuf>,
    /// Start the game through Proton directly instead of asking Steam to launch it.
    pub standalone: bool,
    /// Allow several clients at once, each with its own emulator identity.
    pub dev_mode: bool,
    /// Start one real Steam client and one Goldberg client together.
    pub mixed: bool,
    /// Keep the original exe (PlayFab Party crossplay) instead of patching to Steam.
    pub party: bool,
    /// Mark the intro video and crossplay popup as seen before every launch.
    pub skip_intro: bool,
    /// Where the Steam emulator release lives (auto-detected when unset).
    pub emulator_dir: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            server: "127.0.0.1".into(),
            social_port: 8443,
            proxy_port: 8888,
            game_dir: None,
            standalone: false,
            dev_mode: false,
            mixed: false,
            party: false,
            skip_intro: true,
            emulator_dir: None,
        }
    }
}

/// Where the game and its Proton prefix live.
pub struct Game {
    pub dir: PathBuf,
    pub compat: PathBuf,
}

/// One dev-mode client: its own emulator settings and save directory.
pub struct Player {
    pub name: String,
    pub dir: PathBuf,
    /// 1-based slot, used to derive guest identities from the local Steam account.
    pub slot: usize,
}

/// The Steam account configured in the local Steam client, used to make Goldberg
/// look like the player's real account in standalone mode.
#[derive(Clone, Debug)]
pub struct SteamAccount {
    pub id: u64,
    pub account_name: String,
    pub persona_name: String,
    last_used: i64,
    most_recent: bool,
    auto_login: bool,
    avatar: Option<PathBuf>,
}

impl SteamAccount {
    pub fn display_name(&self) -> &str {
        if !self.persona_name.is_empty() { &self.persona_name } else { &self.account_name }
    }
}

/// What is currently applied, for the UI status line.
pub struct Status {
    pub patched: bool,
    pub ca_thumbprint: Option<String>,
}

#[cfg(windows)]
pub(crate) fn steam_root() -> Result<PathBuf> {
    // HKCU\Software\Valve\Steam\SteamPath, written by the Steam client
    let out = hidden(Command::new("reg"))
        .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let from_reg = out.lines().find_map(|l| l.split("REG_SZ").nth(1)).map(|p| PathBuf::from(p.trim()));
    from_reg
        .into_iter()
        .chain([PathBuf::from(r"C:\Program Files (x86)\Steam")])
        .find(|p| p.join("steamapps").is_dir())
        .ok_or_else(|| "Steam installation not found".into())
}

#[cfg(not(windows))]
pub(crate) fn steam_root() -> Result<PathBuf> {
    let home = PathBuf::from(env::var("HOME").map_err(|_| "HOME not set")?);
    [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"]
        .iter()
        .map(|p| home.join(p))
        .find(|p| p.join("steamapps").is_dir())
        .ok_or_else(|| "Steam installation not found".into())
}

/// Parse Steam's `config/loginusers.vdf` and pick the account Steam would log in as.
fn parse_loginusers(text: &str, avatar_dir: &Path) -> Option<SteamAccount> {
    let mut users: Vec<SteamAccount> = Vec::new();
    let mut cur: Option<SteamAccount> = None;
    for line in text.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        // A user block starts with one quoted 17-digit SteamID64.
        if parts.len() == 3 {
            let token = parts[1].trim();
            if token.len() >= 17 && token.chars().all(|c| c.is_ascii_digit()) {
                if let Some(u) = cur.take() {
                    users.push(u);
                }
                if let Ok(id) = token.parse::<u64>() {
                    let avatar = ["png", "jpg", "jpeg"]
                        .iter()
                        .map(|ext| avatar_dir.join(format!("{id}.{ext}")))
                        .find(|p| p.is_file());
                    cur = Some(SteamAccount {
                        id,
                        account_name: String::new(),
                        persona_name: String::new(),
                        last_used: 0,
                        most_recent: false,
                        auto_login: false,
                        avatar,
                    });
                }
            }
        } else if parts.len() >= 5 {
            let key = parts[1].trim();
            let value = parts[3].trim();
            if let Some(u) = cur.as_mut() {
                match key {
                    "AccountName" => u.account_name = value.into(),
                    "PersonaName" => u.persona_name = value.into(),
                    "timestamp" => u.last_used = value.parse().unwrap_or(0),
                    "MostRecent" => u.most_recent = value == "1",
                    "AutoLogin" => u.auto_login = value == "1",
                    _ => {}
                }
            }
        }
    }
    if let Some(u) = cur {
        users.push(u);
    }
    users.retain(|u| u.id >= 76561197960265728);
    users
        .iter()
        .filter(|u| u.most_recent)
        .max_by_key(|u| u.last_used)
        .or_else(|| users.iter().filter(|u| u.auto_login).max_by_key(|u| u.last_used))
        .or_else(|| users.iter().max_by_key(|u| u.last_used))
        .cloned()
}

/// The account Steam is configured to use, if Steam has been signed in at least once.
pub fn local_steam_account() -> Option<SteamAccount> {
    let root = steam_root().ok()?;
    let text = fs::read_to_string(root.join("config/loginusers.vdf")).ok()?;
    parse_loginusers(&text, &root.join("config/avatarcache"))
}

fn safe_name(name: &str) -> String {
    let name = name.replace(['\r', '\n'], " ").trim().to_string();
    if name.is_empty() { "Steam User".into() } else { name }
}

/// Write Goldberg's per-identity settings. Used for the single standalone client and
/// for each dev slot.
fn write_identity(settings: &Path, name: &str, steamid: u64, avatar: Option<&Path>) -> Result<()> {
    fs::create_dir_all(settings).map_err(|e| format!("create {}: {e}", settings.display()))?;
    let ini = format!(
        "[user::general]\naccount_name={}\naccount_steamid={steamid}\nlanguage=english\n",
        safe_name(name)
    );
    fs::write(settings.join("configs.user.ini"), ini).map_err(|e| format!("write configs.user.ini: {e}"))?;
    if let Some(src) = avatar {
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png");
        let dst = settings.join(format!("account_avatar.{ext}"));
        if fs::copy(src, &dst).is_err() {
            // Avatar is optional; a missing/blocked file must not stop the launch.
        }
    }
    Ok(())
}

/// Library folders from libraryfolders.vdf ("path" entries).
fn libraries(root: &Path) -> Vec<PathBuf> {
    let vdf = fs::read_to_string(root.join("steamapps/libraryfolders.vdf")).unwrap_or_default();
    let mut libs = vec![root.to_path_buf()];
    for line in vdf.lines() {
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() >= 4 && parts[1] == "path" {
            libs.push(PathBuf::from(parts[3].replace("\\\\", "\\")));
        }
    }
    libs
}

pub fn find_game(s: &Settings) -> Result<Game> {
    let (dir, lib) = match &s.game_dir {
        Some(d) => {
            let lib = d.ancestors().nth(3).ok_or("cannot derive Steam library from game dir")?;
            (d.clone(), lib.to_path_buf())
        }
        None => libraries(&steam_root()?)
            .into_iter()
            .map(|l| (l.join("steamapps/common/SecondExtinction"), l))
            .find(|(g, _)| g.join(EXE).is_file())
            .ok_or("Second Extinction not found in any Steam library")?,
    };
    if !dir.join(EXE).is_file() {
        return Err(format!("{EXE} not found in {}", dir.display()));
    }
    Ok(Game { dir, compat: lib.join("steamapps/compatdata").join(APP_ID) })
}

/// A command that does not flash a console window.
#[cfg(windows)]
fn hidden(mut cmd: Command) -> Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

#[cfg(windows)]
fn tasklist(filter: &str) -> String {
    hidden(Command::new("tasklist"))
        .args(["/NH", "/FO", "CSV", "/FI", filter])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(windows)]
pub fn game_running() -> bool {
    tasklist(&format!("IMAGENAME eq {EXE}")).contains(EXE)
}

#[cfg(windows)]
fn steam_game_running() -> bool {
    game_running()
}

#[cfg(not(windows))]
pub fn game_running() -> bool {
    fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| {
        fs::read(e.path().join("cmdline"))
            .map(|c| {
                // argv[0] only, so shells/editors mentioning the name don't count
                let argv0 = c.split(|&b| b == 0).next().unwrap_or_default();
                String::from_utf8_lossy(argv0).replace('\\', "/").ends_with(EXE)
            })
            .unwrap_or(false)
    })
}

/// The real Steam client's game process, not the shadow/Goldberg one.
#[cfg(not(windows))]
fn steam_game_running() -> bool {
    fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| {
        fs::read(e.path().join("cmdline"))
            .map(|c| {
                // argv[0] only: steam.exe and Proton wrappers also mention the exe path.
                let argv0 = c.split(|&b| b == 0).next().unwrap_or_default();
                let s = String::from_utf8_lossy(argv0).replace('\\', "/");
                s.ends_with(EXE) && !s.contains("/mixed/game/")
            })
            .unwrap_or(false)
    })
}

/// Unix path as a Windows path inside Proton's `Z:` drive (which maps to `/`).
#[cfg(not(windows))]
fn wine_path(p: &Path) -> String {
    format!("Z:{}", p.display()).replace('/', "\\")
}

/// Already a Windows path.
#[cfg(windows)]
fn wine_path(p: &Path) -> String {
    p.display().to_string()
}

/// The emulator DLL carries `GseAppPath`/`GseSavePath`; the real Steam DLL doesn't.
fn is_emulator(dll: &Path) -> bool {
    fs::read(dll).map(|d| d.windows(b"GseAppPath".len()).any(|w| w == b"GseAppPath")).unwrap_or(false)
}

fn emulator_candidates(s: &Settings) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(d) = &s.emulator_dir {
        roots.push(d.clone());
    }
    if let Some(d) = env::var_os("SE_STEAM_EMULATOR") {
        roots.push(PathBuf::from(d));
    }
    if let Some(d) = crate::instance_dir() {
        roots.push(d.join("steam-emulator"));
    }
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join("Projects/steam-emulator/release"));
        roots.push(home.join(".local/share/se-launcher/steam-emulator"));
    }
    roots
}

fn find_emulator_dll(s: &Settings) -> Result<PathBuf> {
    for root in emulator_candidates(s) {
        for rel in [
            "regular/x64/steam_api64.dll",
            "experimental/x64/steam_api64.dll",
            "x64/steam_api64.dll",
            "steam_api64.dll",
        ] {
            let p = root.join(rel);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err("Steam emulator not found; set the emulator directory (or SE_STEAM_EMULATOR) to a Goldberg release".into())
}

/// Put the emulator's steam_api64.dll (with a backup of the original) and a basic
/// steam_settings folder into the game directory, so the game can start without Steam.
pub fn install_emulator(g: &Game, s: &Settings, log: &dyn Fn(String)) -> Result<()> {
    let target = g.dir.join("steam_api64.dll");
    if !is_emulator(&target) {
        if game_running() {
            return Err("the Steam emulator must be installed but a game client is running; close it first".into());
        }
        let src = find_emulator_dll(s)?;
        let backup = g.dir.join("steam_api64.dll.orig");
        if target.is_file() && !backup.exists() {
            fs::copy(&target, &backup).map_err(|e| format!("backup steam_api64.dll: {e}"))?;
            log(format!("Backed up original steam_api64.dll to {}", backup.display()));
        }
        fs::copy(&src, &target).map_err(|e| format!("install emulator from {}: {e}", src.display()))?;
        log(format!("Installed Steam emulator {}", src.display()));
    }
    let settings = g.dir.join("steam_settings");
    fs::create_dir_all(&settings).map_err(|e| format!("create steam_settings: {e}"))?;
    let appid = settings.join("steam_appid.txt");
    if !appid.exists() {
        fs::write(&appid, format!("{APP_ID}\n")).map_err(|e| format!("write steam_appid.txt: {e}"))?;
    }
    Ok(())
}

fn restore_emulator(g: &Game, log: &dyn Fn(String)) {
    let backup = g.dir.join("steam_api64.dll.orig");
    if backup.exists() {
        if fs::copy(&backup, g.dir.join("steam_api64.dll")).is_ok() {
            fs::remove_file(&backup).ok();
            log("Restored original steam_api64.dll".into());
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn status(g: &Game) -> Status {
    let patched = fs::read(g.dir.join(EXE))
        .map(|d| PATCHES.iter().all(|&(off, _, new)| d.get(off) == Some(&new)))
        .unwrap_or(false);
    let ca_thumbprint = fs::read_to_string(state_file(&g.compat)).ok().map(|t| t.trim().to_string());
    Status { patched, ca_thumbprint }
}

pub fn patch_exe(g: &Game, log: &dyn Fn(String)) -> Result<()> {
    let exe = g.dir.join(EXE);
    let backup = g.dir.join(format!("{EXE}.orig"));
    let mut data = fs::read(&exe).map_err(|e| format!("read {}: {e}", exe.display()))?;
    if !backup.exists() {
        if hex(&Sha256::digest(&data)) != EXE_SHA256 {
            return Err("exe does not match the analyzed build (game updated?); refusing to patch".into());
        }
        fs::copy(&exe, &backup).map_err(|e| format!("backup: {e}"))?;
        log(format!("Backed up original exe to {}", backup.display()));
    }
    let mut changed = false;
    for &(off, old, new) in PATCHES {
        match data[off] {
            b if b == new => log(format!("Patch at {off:#x} already applied")),
            b if b == old => {
                data[off] = new;
                changed = true;
                log(format!("Patched {off:#x}: {old:#04x} -> {new:#04x} (Steam matchmaking)"));
            }
            b => return Err(format!("unexpected byte {b:#04x} at {off:#x}")),
        }
    }
    // Never touch a running game's executable: it is mapped into the process and
    // truncating/rewriting it can crash the client that is already running.
    if !changed {
        return Ok(());
    }
    if game_running() {
        return Err("the exe needs patching but a game client is running; close it first".into());
    }
    fs::write(&exe, data).map_err(|e| format!("write exe: {e}"))
}

/// Leave the exe untouched so the game chooses the PlayFab Party backend.
pub fn ensure_original_exe(g: &Game, log: &dyn Fn(String)) -> Result<()> {
    let exe = g.dir.join(EXE);
    let backup = g.dir.join(format!("{EXE}.orig"));
    let data = fs::read(&exe).map_err(|e| format!("read {}: {e}", exe.display()))?;
    let patched = PATCHES.iter().all(|&(off, _, new)| data.get(off) == Some(&new));
    if patched && backup.exists() {
        if game_running() {
            return Err("the exe needs restoring but a game client is running; close it first".into());
        }
        fs::copy(&backup, &exe).map_err(|e| format!("restore exe: {e}"))?;
        log("Restored original exe (PlayFab Party crossplay)".into());
    } else if !patched {
        log("Exe already unpatched (PlayFab Party crossplay)".into());
    } else {
        log("Exe is patched and no .orig backup exists; cannot restore it".into());
    }
    Ok(())
}

pub fn revert_exe(g: &Game, log: &dyn Fn(String)) -> Result<()> {
    let backup = g.dir.join(format!("{EXE}.orig"));
    if backup.exists() {
        fs::copy(&backup, g.dir.join(EXE)).map_err(|e| format!("restore: {e}"))?;
        fs::remove_file(&backup).ok();
        log(format!("Restored original {EXE}"));
    } else {
        log("No exe backup, nothing to restore".into());
    }
    Ok(())
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    decode_b64(&b64).ok_or_else(|| "server returned an invalid CA certificate".into())
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let v: Vec<u8> = chunk.iter().map(|&c| val(c)).collect::<Option<_>>()?;
        let n = v.iter().enumerate().fold(0u32, |acc, (i, &x)| acc | (x as u32) << (18 - 6 * i));
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    Some(out)
}

fn reg_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(",")
}

/// Wine registry cert store "Blob": serialized property (id 0x20 = CERT_CERT_PROP_ID, 1, len, DER).
/// WinHttpSettings: magic 0x18, 0, flags 2 (PROXY_TYPE_PROXY), len+proxy, len+bypass.
fn reg_file(ca_der: Option<&[u8]>, thumb: &str, proxy: Option<&str>) -> String {
    let mut s = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
    match ca_der {
        Some(der) => {
            let mut blob = Vec::new();
            for v in [0x20u32, 1, der.len() as u32] {
                blob.extend_from_slice(&v.to_le_bytes());
            }
            blob.extend_from_slice(der);
            s += &format!("[{CERT_KEY}\\{thumb}]\r\n\"Blob\"=hex:{}\r\n\r\n", reg_hex(&blob));
        }
        None if !thumb.is_empty() => s += &format!("[-{CERT_KEY}\\{thumb}]\r\n\r\n"),
        None => {}
    }
    s += &format!("[{CONN_KEY}]\r\n");
    match proxy {
        Some(p) => {
            let mut v = Vec::new();
            for x in [0x18u32, 0, 2, p.len() as u32] {
                v.extend_from_slice(&x.to_le_bytes());
            }
            v.extend_from_slice(p.as_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());
            s += &format!("\"WinHttpSettings\"=hex:{}\r\n", reg_hex(&v));
        }
        None => s += "\"WinHttpSettings\"=-\r\n",
    }
    s
}

#[cfg(not(windows))]
/// Proton installation for this prefix, from compatdata/<app>/config_info
/// (line 2 = fonts dir, e.g. <proton>/files/share/fonts/).
fn proton_root(compat: &Path) -> Result<PathBuf> {
    let info = fs::read_to_string(compat.join("config_info"))
        .map_err(|_| "no Proton prefix yet; start the game once through Steam first")?;
    let fonts = info.lines().nth(1).ok_or("unexpected config_info format")?;
    let files = Path::new(fonts.trim_end_matches('/')).ancestors().nth(2).ok_or("bad fonts path")?;
    let root = files.parent().ok_or("bad Proton path")?;
    if !root.join("proton").is_file() {
        return Err(format!("Proton not found at {}", root.display()));
    }
    Ok(root.to_path_buf())
}

/// Proton's wine binary for this prefix.
#[cfg(not(windows))]
fn proton_wine(compat: &Path) -> Result<PathBuf> {
    let wine = proton_root(compat)?.join("files/bin/wine");
    if !wine.is_file() {
        return Err(format!("Proton wine not found at {}", wine.display()));
    }
    Ok(wine)
}

fn reg_utf16(reg: &str) -> Vec<u8> {
    [0xFEFFu16].into_iter().chain(reg.encode_utf16()).flat_map(|c| c.to_le_bytes()).collect()
}

/// Native Windows: import into the real registry. HKLM needs elevation, so a
/// failed import is retried through a UAC prompt.
#[cfg(windows)]
fn import_reg(_compat: &Path, reg: &str) -> Result<()> {
    let tmp = env::temp_dir().join(format!("se-launcher-{}.reg", std::process::id()));
    fs::write(&tmp, reg_utf16(reg)).map_err(|e| format!("write reg: {e}"))?;
    let ok = hidden(Command::new("reg")).arg("import").arg(&tmp).status().is_ok_and(|s| s.success());
    let ok = ok || hidden(Command::new("powershell"))
        .args(["-NoProfile", "-Command"])
        .arg(format!(
            "$p = Start-Process reg -ArgumentList 'import','\"{}\"' -Verb RunAs -Wait -PassThru -WindowStyle Hidden; exit $p.ExitCode",
            tmp.display()
        ))
        .status()
        .is_ok_and(|s| s.success());
    fs::remove_file(&tmp).ok();
    if !ok {
        return Err("registry import failed (administrator rights are needed for the CA and WinHTTP proxy)".into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn import_reg(compat: &Path, reg: &str) -> Result<()> {
    let wine = proton_wine(compat)?;
    let tmp = env::temp_dir().join(format!("se-launcher-{}.reg", std::process::id()));
    fs::write(&tmp, reg_utf16(reg)).map_err(|e| format!("write reg: {e}"))?;
    let status = Command::new(&wine)
        .env("WINEPREFIX", compat.join("pfx"))
        .env("WINEDEBUG", "-all")
        .args(["regedit", "/S"])
        .arg(format!("Z:{}", tmp.display()))
        .status();
    fs::remove_file(&tmp).ok();
    if !status.map_err(|e| format!("run wine: {e}"))?.success() {
        return Err("regedit failed".into());
    }
    // let wineserver flush the registry to disk
    Command::new(wine.with_file_name("wineserver")).env("WINEPREFIX", compat.join("pfx")).arg("-w").status().ok();
    Ok(())
}

#[cfg(not(windows))]
fn state_file(compat: &Path) -> PathBuf {
    compat.join("se-launcher-ca.sha1")
}

/// There is no prefix on Windows; keep the state with the launcher's data.
#[cfg(windows)]
fn state_file(_compat: &Path) -> PathBuf {
    let dir = crate::instance_dir().map(Path::to_path_buf).unwrap_or_else(env::temp_dir);
    fs::create_dir_all(&dir).ok();
    dir.join("se-launcher-ca.sha1")
}

/// The game keeps the "already seen" flags in its per-prefix settings.json.
/// Every fresh Proton prefix starts without them and replays the intro video,
/// so the launcher marks them before each launch.
fn settings_files(compat: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(windows)]
    let users: Vec<PathBuf> = {
        let _ = compat;
        env::var_os("USERPROFILE").map(PathBuf::from).into_iter().collect()
    };
    #[cfg(not(windows))]
    let users: Vec<PathBuf> = match fs::read_dir(compat.join("pfx/drive_c/users")) {
        Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
        Err(_) => return out,
    };
    for user in users {
        let base = user.join("Saved Games/Avalanche Studios/Second Extinction");
        let Ok(platforms) = fs::read_dir(&base) else { continue };
        for platform in platforms.flatten() {
            let file = platform.path().join("settings.json");
            if file.is_file() {
                out.push(file);
            }
        }
    }
    out
}

/// The game writes settings.json with CRLF and its parser trips over LF-only
/// files (a truncated-looking line produces an invalid string and a crash).
/// Keep every line ending CRLF when editing the document.
fn with_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Set `"key": 1` in a small settings document without pulling in a JSON crate.
/// The game writes one key per line; inserting after the opening brace keeps
/// that layout and existing values are rewritten in place.
fn set_json_flag(text: &str, key: &str) -> (String, bool) {
    let needle = format!("\"{key}\"");
    if let Some(pos) = text.find(&needle) {
        let Some(colon) = text[pos + needle.len()..].find(':') else {
            return (text.to_string(), false);
        };
        let after = pos + needle.len() + colon + 1;
        let start = after + text[after..].find(|c: char| !c.is_whitespace()).unwrap_or(0);
        let end = text[start..]
            .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
            .map(|i| start + i)
            .unwrap_or(text.len());
        if text[start..end] == *"1" {
            return (text.to_string(), false);
        }
        let mut out = String::with_capacity(text.len() + 1);
        out.push_str(&text[..start]);
        out.push('1');
        out.push_str(&text[end..]);
        return (out, true);
    }
    let Some(brace) = text.find('{') else { return (text.to_string(), false) };
    let mut out = String::with_capacity(text.len() + key.len() + 16);
    out.push_str(&text[..=brace]);
    out.push_str(&format!("\r\n    \"{key}\": 1,"));
    out.push_str(&text[brace + 1..]);
    (out, true)
}

fn skip_intro(compat: &Path, log: &dyn Fn(String)) -> Result<()> {
    for path in settings_files(compat) {
        let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut out = with_crlf(&text);
        let mut changed = out != text;
        for key in ["SeenIntroVideo", "HasSeenCrossplayPopup"] {
            let (next, edited) = set_json_flag(&out, key);
            out = next;
            changed |= edited;
        }
        if changed {
            // Replace atomically: a partially written settings file makes the
            // game crash while reading it.
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, out).map_err(|e| format!("write {}: {e}", tmp.display()))?;
            fs::rename(&tmp, &path).map_err(|e| format!("replace {}: {e}", path.display()))?;
            log(format!("Intro skip enabled in {}", path.display()));
        }
    }
    Ok(())
}

pub fn setup_prefix(g: &Game, s: &Settings, log: &dyn Fn(String)) -> Result<()> {
    if s.skip_intro {
        skip_intro(&g.compat, log)?;
    }
    let url = format!("http://{}:{}/ca.pem", s.server, s.social_port);
    log(format!("Fetching server CA from {url}"));
    let pem = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .map_err(|e| format!("cannot reach server ({url}): {e}"))?
        .into_string()
        .map_err(|e| format!("read CA: {e}"))?;
    let der = pem_to_der(&pem)?;
    let thumb = hex(&Sha1::digest(&der)).to_uppercase();
    let proxy = format!("{}:{}", s.server, s.proxy_port);
    let previous = fs::read_to_string(state_file(&g.compat)).unwrap_or_default();
    let mut lines = previous.lines();
    let old_thumb = lines.next().unwrap_or_default().trim();
    let old_proxy = lines.next().unwrap_or_default().trim();
    // Already configured for this server and proxy: leave the running prefix alone.
    // A one-line state file is the legacy format, written after a successful import.
    if old_thumb == thumb && (old_proxy.is_empty() || old_proxy == proxy) {
        if old_proxy.is_empty() {
            fs::write(state_file(&g.compat), format!("{thumb}\n{proxy}\n")).ok();
        }
        log(format!("Prefix already trusts CA {thumb} and proxies through {proxy}"));
        return Ok(());
    }
    if game_running() {
        return Err("the server or proxy changed but a game client is running; close it first".into());
    }
    // drop a CA from a previous server before trusting the new one
    if !old_thumb.is_empty() && old_thumb != thumb {
        log(format!("Removing previous server CA {old_thumb}"));
        import_reg(&g.compat, &reg_file(None, old_thumb, Some(&proxy)))?;
    }
    log("Updating Proton prefix (this can take a few seconds)".into());
    import_reg(&g.compat, &reg_file(Some(&der), &thumb, Some(&proxy)))?;
    fs::write(state_file(&g.compat), format!("{thumb}\n{proxy}\n")).ok();
    log(format!("Prefix trusts CA {thumb}, WinHTTP proxy {proxy}"));
    Ok(())
}

pub fn revert_prefix(g: &Game, log: &dyn Fn(String)) -> Result<()> {
    let thumb = fs::read_to_string(state_file(&g.compat)).unwrap_or_default();
    import_reg(&g.compat, &reg_file(None, thumb.trim(), None))?;
    fs::remove_file(state_file(&g.compat)).ok();
    log("Removed server CA and WinHTTP proxy from the prefix".into());
    Ok(())
}

pub fn launch_args(s: &Settings) -> Vec<String> {
    let v: Vec<String> = ["--social-host", &s.server, "--social-port", &s.social_port.to_string(), "--social-disable-ssl"]
        .iter()
        .map(|a| a.to_string())
        .collect();
    // The game reads CrossplayEnabled from settings.json; the command-line flag
    // forces the PlayFab online-state path even on the Steam client, which leaves
    // the Steam session object null and stalls the lobby. Keep the setting, not
    // the forced flag.
    v
}

pub fn launch(g: &Game, s: &Settings, player: Option<&Player>, log: &dyn Fn(String)) -> Result<()> {
    let args = launch_args(s);
    if s.standalone || s.dev_mode {
        return launch_standalone(g, player, &args, log);
    }
    launch_via_steam(&args, log)
}

fn launch_via_steam(args: &[String], log: &dyn Fn(String)) -> Result<()> {
    #[cfg(windows)]
    let steam = steam_root()?.join("steam.exe");
    #[cfg(not(windows))]
    let steam = PathBuf::from("steam");
    Command::new(&steam)
        .args(["-applaunch", APP_ID])
        .args(args)
        .spawn()
        .map_err(|e| format!("start steam: {e}"))?;
    log(format!("Started: steam -applaunch {APP_ID} {}", args.join(" ")));
    Ok(())
}

/// Start the exe directly; the Steam emulator provides steam_api64.dll.
#[cfg(windows)]
fn launch_standalone(g: &Game, player: Option<&Player>, args: &[String], log: &dyn Fn(String)) -> Result<()> {
    let mut cmd = Command::new(g.dir.join(EXE));
    cmd.current_dir(&g.dir).env("SteamAppId", APP_ID).env("SteamGameId", APP_ID).args(args);
    if let Some(p) = player {
        cmd.env("GseAppPath", wine_path(&p.dir))
            .env("GseSavePath", wine_path(&p.dir.join("saves")))
            .env("GseExeDir", wine_path(&g.dir));
    }
    let child = cmd.spawn().map_err(|e| format!("start {EXE}: {e}"))?;
    if let Some(p) = player {
        fs::write(p.dir.join("pid"), format!("{}\n", child.id())).ok();
    }
    log(format!("Started (standalone): {EXE} {}", args.join(" ")));
    Ok(())
}

#[cfg(not(windows))]
/// Start the exe through Proton without Steam. Proton's launcher sets up the
/// prefix and the Wine environment itself; the real Steam client is never used,
/// so the Steam emulator must provide the game's steam_api64.dll.
fn launch_standalone(g: &Game, player: Option<&Player>, args: &[String], log: &dyn Fn(String)) -> Result<()> {
    let proton = proton_root(&g.compat)?.join("proton");
    let lib = g.compat.ancestors().nth(3).ok_or("cannot derive the Steam library from the prefix")?;
    let client = steam_root().unwrap_or_else(|_| lib.to_path_buf());
    let mut cmd = Command::new(&proton);
    cmd.current_dir(&g.dir)
        .env("STEAM_COMPAT_DATA_PATH", &g.compat)
        .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &client)
        .env("STEAM_COMPAT_INSTALL_PATH", &g.dir)
        .env("STEAM_COMPAT_LIBRARY_PATHS", lib)
        .env("SteamAppId", APP_ID)
        .env("SteamGameId", APP_ID)
        .env("SteamOverlayGameId", APP_ID)
        .arg("run")
        .arg(g.dir.join(EXE))
        .args(args);
    // Goldberg reads its settings from GseAppPath and saves to GseSavePath, so each
    // dev player gets a distinct identity and save folder from the same install.
    if let Some(p) = player {
        cmd.env("GseAppPath", wine_path(&p.dir))
            .env("GseSavePath", wine_path(&p.dir.join("saves")))
            .env("GseExeDir", wine_path(&g.dir));
    }
    let child = cmd.spawn().map_err(|e| format!("start Proton: {e}"))?;
    if let Some(p) = player {
        fs::write(p.dir.join("pid"), format!("{}\n", child.id())).ok();
    }
    match player {
        Some(p) => log(format!("Started dev player {}: {} run {EXE} {}", p.name, proton.display(), args.join(" "))),
        None => log(format!("Started (standalone): {} run {EXE} {}", proton.display(), args.join(" "))),
    }
    Ok(())
}

#[cfg(windows)]
fn player_running(dir: &Path) -> bool {
    let pid = fs::read_to_string(dir.join("pid")).ok().and_then(|t| t.trim().parse::<u32>().ok());
    match pid {
        Some(pid) if tasklist(&format!("PID eq {pid}")).contains(&format!("\"{pid}\"")) => true,
        _ => {
            fs::remove_file(dir.join("pid")).ok();
            false
        }
    }
}

/// True while a process has this player's emulator settings path in its environment,
/// or while the Proton process recorded for the slot is still alive.
#[cfg(not(windows))]
fn player_running(dir: &Path) -> bool {
    if let Ok(text) = fs::read_to_string(dir.join("pid")) {
        if let Ok(pid) = text.trim().parse::<u32>() {
            // /proc/<pid>/stat: "pid (comm) state ...", comm may contain spaces/parens
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                if stat.rsplit(')').next().and_then(|s| s.split_whitespace().next()) != Some("Z") {
                    return true;
                }
            }
        }
        fs::remove_file(dir.join("pid")).ok();
    }
    let needle = format!("GseAppPath={}", wine_path(dir));
    fs::read_dir("/proc").into_iter().flatten().flatten().any(|e| {
        let name = e.file_name();
        if !name.to_str().is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) {
            return false;
        }
        fs::read(e.path().join("environ"))
            .map(|env| env.split(|&b| b == 0).any(|v| v == needle.as_bytes()))
            .unwrap_or(false)
    })
}

const STEAMID_BASE: u64 = 76561197960287930;

/// Allocate the next free dev player. With `--instance-id` the first slot is named
/// after the instance, so separate launcher profiles map to stable players.
fn next_player() -> Result<Player> {
    let root = crate::instance_dir().ok_or("launcher instance not initialized")?.join("players");
    fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    let base = crate::instance_id();
    for n in 1..=99 {
        let (name, id) = match base {
            Some(id) if n == 1 => (id.to_string(), id.to_string()),
            Some(id) => (format!("{id} #{n}"), format!("{id}-{n}")),
            None => (format!("Player {n}"), format!("player-{n}")),
        };
        let dir = root.join(id);
        if !player_running(&dir) {
            return Ok(Player { name, dir, slot: n });
        }
    }
    Err("no free dev player slot; close a client first".into())
}

/// A stable SteamID per player slot, so its saves and server progress stay attached
/// to that player across launches.
fn player_steamid(dir: &Path) -> Result<u64> {
    let file = dir.join("steamid");
    if let Ok(text) = fs::read_to_string(&file) {
        if let Ok(id) = text.trim().parse::<u64>() {
            return Ok(id);
        }
    }
    let mut next = STEAMID_BASE + 1;
    for entry in fs::read_dir(dir.parent().unwrap_or(dir)).into_iter().flatten().flatten() {
        if let Ok(text) = fs::read_to_string(entry.path().join("steamid")) {
            if let Ok(id) = text.trim().parse::<u64>() {
                next = next.max(id + 1);
            }
        }
    }
    fs::write(&file, format!("{next}\n")).map_err(|e| format!("write {}: {e}", file.display()))?;
    Ok(next)
}

/// Dev players need separate Wine prefixes: the game keeps a single-instance lock
/// (and its own settings) per prefix, so sharing one makes the second client refuse
/// to start. The first launch for a player copies the already-configured prefix.
/// No Wine prefixes on Windows: every client uses the real user profile.
#[cfg(windows)]
fn clone_prefix(_src: &Path, _dest: &Path, _log: &dyn Fn(String)) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn clone_prefix(src: &Path, dest: &Path, log: &dyn Fn(String)) -> Result<()> {
    if dest.join("pfx").is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    log(format!("Creating isolated Proton prefix for this player (copying {})", src.display()));
    let out = Command::new("cp")
        .args(["-a", "--reflink=auto"])
        .arg(format!("{}/.", src.display()))
        .arg(dest)
        .output()
        .map_err(|e| format!("copy prefix: {e}"))?;
    if !out.status.success() {
        fs::remove_dir_all(dest).ok();
        return Err(format!("copy prefix failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    log("Isolated prefix ready".into());
    Ok(())
}

fn party_entity(steamid: u64) -> String {
    format!("{steamid:016X}")
}

fn write_party_identity(dir: &Path, entity: &str, log: &dyn Fn(String)) -> Result<()> {
    fs::write(dir.join("party_identity.txt"), format!("{entity}\n"))
        .map_err(|e| format!("write party_identity.txt in {}: {e}", dir.display()))?;
    log(format!("Party identity {entity} written to {}", dir.display()));
    Ok(())
}

fn prepare_player(p: &Player, local: Option<&SteamAccount>, log: &dyn Fn(String)) -> Result<()> {
    let settings = p.dir.join("steam_settings");
    fs::create_dir_all(&settings).map_err(|e| format!("create {}: {e}", settings.display()))?;
    fs::write(settings.join("steam_appid.txt"), format!("{APP_ID}\n"))
        .map_err(|e| format!("write steam_appid.txt: {e}"))?;

    // With no --instance-id, the first dev client impersonates the Steam account that is
    // signed in locally. Extra clients get the same persona plus a unique SteamID64 so
    // they can still discover and join each other.
    let (name, steamid) = match local.filter(|_| crate::instance_id().is_none()) {
        Some(a) => {
            let steamid = a.id.saturating_add((p.slot - 1) as u64);
            let name = if p.slot == 1 { a.display_name().to_string() } else { format!("{} #{}", a.display_name(), p.slot) };
            let previous = fs::read_to_string(p.dir.join("steamid")).unwrap_or_default();
            let previous = previous.trim();
            if !previous.is_empty() && previous != steamid.to_string() {
                log(format!("Player slot {} identity changed: {previous} -> {steamid}", p.slot));
            }
            fs::write(p.dir.join("steamid"), format!("{steamid}\n"))
                .map_err(|e| format!("write steamid: {e}"))?;
            write_identity(&settings, &name, steamid, a.avatar.as_deref())?;
            log(format!("Dev player {} mimics Steam account {} ({steamid})", p.slot, name));
            (name, steamid)
        }
        None => {
            let steamid = player_steamid(&p.dir)?;
            write_identity(&settings, &p.name, steamid, None)?;
            (p.name.clone(), steamid)
        }
    };

    // Goldberg discovers peers by broadcasting to the default port (47584). Two clients
    // on one machine cannot both bind it, so the second falls back to 47585 and the
    // broadcasts never meet. Targeting 127.0.0.1 on the default port makes the client
    // on 47585 reach the one on 47584 (and its reply comes back to 47585).
    fs::write(settings.join("custom_broadcasts.txt"), "127.0.0.1:47584\n")
        .map_err(|e| format!("write custom_broadcasts.txt: {e}"))?;
    fs::create_dir_all(p.dir.join("saves")).map_err(|e| format!("create player saves: {e}"))?;
    log(format!("Dev player {name} (SteamID {steamid}) data in {}", p.dir.display()));
    Ok(())
}

/// Mixed mode: the real Steam client keeps the real account, so the Goldberg client
/// must use a distinct guest identity. Do not reuse `players/.../steamid` here, because
/// that file may hold the real account from an earlier single-client simulation.
fn prepare_guest_player(p: &Player, log: &dyn Fn(String)) -> Result<()> {
    let settings = p.dir.join("steam_settings");
    fs::create_dir_all(&settings).map_err(|e| format!("create {}: {e}", settings.display()))?;
    fs::write(settings.join("steam_appid.txt"), format!("{APP_ID}\n"))
        .map_err(|e| format!("write steam_appid.txt: {e}"))?;
    let steamid = STEAMID_BASE + 1000 + p.slot as u64;
    let name = format!("{} (guest)", p.name);
    fs::write(p.dir.join("steamid"), format!("{steamid}\n"))
        .map_err(|e| format!("write steamid: {e}"))?;
    write_identity(&settings, &name, steamid, None)?;
    fs::write(settings.join("custom_broadcasts.txt"), "127.0.0.1:47584\n")
        .map_err(|e| format!("write custom_broadcasts.txt: {e}"))?;
    fs::create_dir_all(p.dir.join("saves")).map_err(|e| format!("create player saves: {e}"))?;
    log(format!("Mixed guest {name} (SteamID {steamid}) data in {}", p.dir.display()));
    Ok(())
}

/// Single standalone client: write the emulator identity into the game's own
/// `steam_settings` folder (no per-player GseAppPath is set in this mode).
pub fn prepare_standalone_identity(g: &Game, local: Option<&SteamAccount>, log: &dyn Fn(String)) -> Result<()> {
    let settings = g.dir.join("steam_settings");
    fs::create_dir_all(&settings).map_err(|e| format!("create {}: {e}", settings.display()))?;
    if !settings.join("steam_appid.txt").is_file() {
        fs::write(settings.join("steam_appid.txt"), format!("{APP_ID}\n"))
            .map_err(|e| format!("write steam_appid.txt: {e}"))?;
    }
    match local {
        Some(a) => {
            write_identity(&settings, a.display_name(), a.id, a.avatar.as_deref())?;
            log(format!("Simulating local Steam account {} ({})", a.display_name(), a.id));
        }
        None => log("No signed-in Steam account found; using the emulator's default identity".into()),
    }
    Ok(())
}

#[cfg(not(windows))]
fn link_into_shadow(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(src, dest)
}

/// Symlinks need extra rights on Windows: hard-link files (same volume),
/// and junction-free directory links are not needed by the game.
#[cfg(windows)]
fn link_into_shadow(src: &Path, dest: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dest)
    } else {
        fs::hard_link(src, dest).or_else(|_| fs::copy(src, dest).map(|_| ()))
    }
}

fn mixed_game_dir() -> Result<PathBuf> {
    let root = crate::instance_dir().ok_or("launcher instance not initialized")?;
    Ok(root.join("mixed/game"))
}

/// Shadow game directory for the Goldberg client: symlink every game file, but
/// give it its own `steam_api64.dll`. That lets a real Steam client and a Goldberg
/// client run side by side from the same install.
fn sync_shadow(src: &Path, shadow: &Path, emulator: &Path, log: &dyn Fn(String)) -> Result<()> {
    if shadow.exists() {
        fs::remove_dir_all(shadow).map_err(|e| format!("clear {}: {e}", shadow.display()))?;
    }
    fs::create_dir_all(shadow).map_err(|e| format!("create {}: {e}", shadow.display()))?;
    for entry in fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "steam_api64.dll"
            || name == "steam_api64.dll.orig"
            || name == "steam_settings"
            || name == "party_identity.txt"
            || name.ends_with(".orig")
        {
            continue;
        }
        link_into_shadow(&entry.path(), &shadow.join(name.as_ref())).map_err(|e| format!("link {name}: {e}"))?;
    }
    fs::copy(emulator, shadow.join("steam_api64.dll")).map_err(|e| format!("install emulator in shadow: {e}"))?;
    // The guest is PlayFab-only: give its exe the original PlayFab Party byte
    // instead of sharing the Steam client's patched executable.
    if src.join(EXE).exists() {
        let exe = shadow.join(EXE);
        fs::remove_file(&exe).ok();
        fs::copy(src.join(EXE), &exe).map_err(|e| format!("copy shadow exe: {e}"))?;
        let mut data = fs::read(&exe).map_err(|e| format!("read shadow exe: {e}"))?;
        data[CROSSPLAY_OFFSET] = 1;
        fs::write(&exe, data).map_err(|e| format!("patch shadow exe: {e}"))?;
    }
    let settings = shadow.join("steam_settings");
    fs::create_dir_all(&settings).map_err(|e| format!("create shadow steam_settings: {e}"))?;
    fs::write(settings.join("steam_appid.txt"), format!("{APP_ID}\n"))
        .map_err(|e| format!("write shadow steam_appid.txt: {e}"))?;
    log(format!("Mixed game shadow ready at {}", shadow.display()));
    Ok(())
}

/// One real Steam client plus one Goldberg client, each with its own prefix and
/// steam_api64.dll. Game files are shared through symlinks.
fn play_mixed(s: &Settings, log: &dyn Fn(String)) -> Result<()> {
    if game_running() {
        return Err("Second Extinction is already running; close it first".into());
    }
    let g = find_game(s)?;
    log(format!("Game: {}", g.dir.display()));
    // Party mode keeps the original exe so the Steam client also takes the PlayFab
    // Party path; otherwise it keeps the Steam session backend (patched byte), while
    // --enablecrossplay makes the Party provider available for crossplay.
    if s.party {
        ensure_original_exe(&g, log)?;
    } else {
        patch_exe(&g, log)?;
    }
    restore_emulator(&g, log);
    if is_emulator(&g.dir.join("steam_api64.dll")) {
        return Err("main steam_api64.dll is still the emulator; restore the original DLL first".into());
    }
    setup_prefix(&g, s, log)?;

    let player = next_player()?;
    // The Steam client already uses the real account; this client must be a guest.
    prepare_guest_player(&player, log)?;
    if let Some(account) = local_steam_account() {
        write_party_identity(&g.dir, &party_entity(account.id), log)?;
    }
    let compat = player.dir.join("compatdata").join(APP_ID);
    clone_prefix(&g.compat, &compat, log)?;

    let shadow = mixed_game_dir()?;
    let emu_dll = find_emulator_dll(s)?;
    sync_shadow(&g.dir, &shadow, &emu_dll, log)?;
    let guest_steamid = STEAMID_BASE + 1000 + player.slot as u64;
    write_party_identity(&shadow, &party_entity(guest_steamid), log)?;
    let emu_game = Game { dir: shadow, compat };
    setup_prefix(&emu_game, s, log)?;

    let args = launch_args(s);
    // Steam refuses to launch while any SecondExtinction process is running, including
    // the Wine steam.exe of the Goldberg client. Start Steam first and wait for its
    // game process before starting the shadow client.
    launch_via_steam(&args, log)?;
    log("Waiting for the Steam client before starting the emulated client...".into());
    let mut seen = false;
    for _ in 0..60 {
        if steam_game_running() {
            seen = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    if !seen {
        log("warning: Steam client not detected after 60s; starting the emulated client anyway".into());
    }
    launch_standalone(&emu_game, Some(&player), &args, log)?;
    log("Mixed test started: one real Steam client + one Goldberg client".into());
    Ok(())
}

/// Patch, set up the prefix and start the game.
pub fn play(s: &Settings, log: &dyn Fn(String)) -> Result<()> {
    if s.mixed {
        return play_mixed(s, log);
    }
    if !s.dev_mode && game_running() {
        return Err("Second Extinction is already running; close it first".into());
    }
    let g = find_game(s)?;
    log(format!("Game: {}", g.dir.display()));
    if let Some(account) = local_steam_account() {
        write_party_identity(&g.dir, &party_entity(account.id), log)?;
    }
    if s.party {
        ensure_original_exe(&g, log)?;
    } else {
        patch_exe(&g, log)?;
    }
    if s.standalone || s.dev_mode {
        install_emulator(&g, s, log)?;
    } else {
        // Switching back to Steam must not keep the Goldberg DLL: Steam will load
        // steam_api64.dll itself and the emulator would hijack the real session.
        restore_emulator(&g, log);
    }
    setup_prefix(&g, s, log)?;
    let local = if s.standalone || s.dev_mode { local_steam_account() } else { None };
    if s.dev_mode {
        let player = next_player()?;
        prepare_player(&player, local.as_ref(), log)?;
        let compat = player.dir.join("compatdata").join(APP_ID);
        clone_prefix(&g.compat, &compat, log)?;
        let player_game = Game { dir: g.dir.clone(), compat };
        setup_prefix(&player_game, s, log)?;
        launch(&player_game, s, Some(&player), log)
    } else {
        if s.standalone {
            prepare_standalone_identity(&g, local.as_ref(), log)?;
        }
        launch(&g, s, None, log)
    }
}

/// Undo the exe patch, the emulator install and prefix changes.
pub fn revert(s: &Settings, log: &dyn Fn(String)) -> Result<()> {
    if game_running() {
        return Err("Second Extinction is running; close it first".into());
    }
    let g = find_game(s)?;
    revert_exe(&g, log)?;
    restore_emulator(&g, log);
    revert_prefix(&g, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intro_flag_is_added_or_flipped() {
        let (text, changed) = set_json_flag("{\r\n    \"Version\": 3,\r\n    \"SeenIntroVideo\": 0\r\n}\r\n", "SeenIntroVideo");
        assert!(changed);
        assert!(text.contains("\"SeenIntroVideo\": 1"));
        assert!(text.contains("\"Version\": 3"));
        assert!(!text.contains("\n") || text.contains("\r\n"));
        let (text, changed) = set_json_flag(&text, "SeenIntroVideo");
        assert!(!changed);
        let (text, changed) = set_json_flag("{\r\n    \"Version\": 3\r\n}\r\n", "HasSeenCrossplayPopup");
        assert!(changed);
        assert!(text.starts_with("{\r\n    \"HasSeenCrossplayPopup\": 1,"));
    }

    #[test]
    fn settings_keep_crlf() {
        assert_eq!(with_crlf("{\n  \"Version\": 3\n}\n"), "{\r\n  \"Version\": 3\r\n}\r\n");
        assert_eq!(with_crlf("{\r\n  \"Version\": 3\r\n}\r\n"), "{\r\n  \"Version\": 3\r\n}\r\n");
    }

    #[test]
    fn unix_paths_become_z_drive_paths() {
        assert_eq!(wine_path(Path::new("/home/x/players/player-1")), r"Z:\home\x\players\player-1");
    }

    #[test]
    fn player_steamids_are_stable_and_unique() {
        let root = env::temp_dir().join(format!("se-launcher-test-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();

        let first = player_steamid(&a).unwrap();
        assert_ne!(first, player_steamid(&b).unwrap());
        assert_eq!(first, player_steamid(&a).unwrap());
        assert!(first > STEAMID_BASE);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn player_slot_pid_is_detected() {
        let dir = env::temp_dir().join(format!("se-launcher-slot-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(!player_running(&dir));

        fs::write(dir.join("pid"), format!("{}\n", std::process::id())).unwrap();
        assert!(player_running(&dir));

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn dev_players_get_separate_identity_and_saves() {
        let root = env::temp_dir().join(format!("se-launcher-dev-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        crate::init_instance(Some(root.clone()), Some("alice".into())).unwrap();

        let first = next_player().unwrap();
        assert_eq!(first.name, "alice");
        prepare_player(&first, None, &|_| {}).unwrap();
        let ini = fs::read_to_string(first.dir.join("steam_settings/configs.user.ini")).unwrap();
        assert!(ini.contains("account_name=alice"));
        assert!(ini.contains("account_steamid=7656"));
        assert!(first.dir.join("saves").is_dir());
        assert_eq!(
            fs::read_to_string(first.dir.join("steam_settings/custom_broadcasts.txt")).unwrap(),
            "127.0.0.1:47584\n"
        );

        // pretend the first client is still running, the next PLAY gets a new slot
        fs::write(first.dir.join("pid"), format!("{}\n", std::process::id())).unwrap();
        let second = next_player().unwrap();
        assert_eq!(second.name, "alice #2");
        assert_ne!(first.dir, second.dir);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    #[ignore = "requires a signed-in Steam client"]
    fn local_steam_account_is_detected() {
        let a = local_steam_account().expect("no Steam account found");
        assert!(a.id > 76561197960265728);
        println!("local Steam account: {} ({})", a.display_name(), a.id);
    }

    #[test]
    fn picks_the_most_recent_steam_account() {
        let text = r#"
"users"
{
    "76561197960287931"
    {
        "AccountName" "old"
        "PersonaName" "Old Name"
        "timestamp" "100"
    }
    "76561197960287932"
    {
        "AccountName" "steamuser"
        "PersonaName" "Player"
        "MostRecent" "1"
        "timestamp" "200"
    }
}
"#;
        let dir = env::temp_dir().join(format!("se-launcher-vdf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = parse_loginusers(text, &dir).unwrap();
        assert_eq!(a.id, 76561197960287932);
        assert_eq!(a.account_name, "steamuser");
        assert_eq!(a.display_name(), "Player");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn identity_ini_and_avatar_are_written() {
        let dir = env::temp_dir().join(format!("se-launcher-id-{}", std::process::id()));
        let settings = dir.join("steam_settings");
        let avatar = dir.join("7656.png");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&avatar, b"png").unwrap();
        write_identity(&settings, "Player", 76561197960287932, Some(&avatar)).unwrap();
        let ini = fs::read_to_string(settings.join("configs.user.ini")).unwrap();
        assert!(ini.contains("account_name=Player"));
        assert!(ini.contains("account_steamid=76561197960287932"));
        assert_eq!(fs::read(settings.join("account_avatar.png")).unwrap(), b"png");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn mixed_shadow_has_its_own_dll_and_links_the_rest() {
        let root = env::temp_dir().join(format!("se-launcher-shadow-{}", std::process::id()));
        let src = root.join("src");
        let shadow = root.join("shadow");
        fs::create_dir_all(src.join("steam_settings")).unwrap();
        fs::write(src.join("data.pak"), b"pak").unwrap();
        fs::write(src.join("steam_api64.dll"), b"steam").unwrap();
        fs::write(src.join("steam_api64.dll.orig"), b"backup").unwrap();
        fs::write(src.join("steam_settings/keep"), b"x").unwrap();
        let emu = root.join("emu.dll");
        fs::write(&emu, b"goldberg").unwrap();

        sync_shadow(&src, &shadow, &emu, &|_| {}).unwrap();
        assert_eq!(fs::read(shadow.join("data.pak")).unwrap(), b"pak");
        assert_eq!(fs::read(shadow.join("steam_api64.dll")).unwrap(), b"goldberg");
        assert!(!shadow.join("steam_api64.dll.orig").exists());
        assert!(shadow.join("steam_settings/steam_appid.txt").is_file());
        assert!(!shadow.join("steam_settings/keep").exists());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn party_mode_restores_the_original_exe() {
        if game_running() {
            // Never rewrite an exe while a real client is running (the test would
            // otherwise trip the same safety check).
            return;
        }
        let dir = env::temp_dir().join(format!("se-launcher-party-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut exe = vec![0u8; 0x34A4AA1];
        exe[0x34A4AA0] = 0; // patched (Steam)
        fs::write(dir.join(EXE), &exe).unwrap();
        exe[0x34A4AA0] = 1; // original (Party)
        fs::write(dir.join(format!("{EXE}.orig")), &exe).unwrap();
        let g = Game { dir: dir.clone(), compat: dir.join("compat") };
        ensure_original_exe(&g, &|_| {}).unwrap();
        assert_eq!(fs::read(dir.join(EXE)).unwrap()[0x34A4AA0], 1);
        assert!(dir.join(format!("{EXE}.orig")).exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn switching_to_steam_restores_original_dll() {
        let dir = env::temp_dir().join(format!("se-launcher-dll-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("steam_api64.dll");
        let backup = dir.join("steam_api64.dll.orig");
        fs::write(&target, b"goldberg GseAppPath").unwrap();
        fs::write(&backup, b"original steam").unwrap();

        let g = Game { dir: dir.clone(), compat: dir.join("compat") };
        restore_emulator(&g, &|_| {});

        assert_eq!(fs::read(&target).unwrap(), b"original steam");
        assert!(!backup.exists());

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn prefix_clone_copies_and_is_idempotent() {
        let root = env::temp_dir().join(format!("se-launcher-prefix-{}", std::process::id()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(src.join("pfx/drive_c")).unwrap();
        fs::write(src.join("config_info"), "proton\n").unwrap();
        fs::write(src.join("pfx/drive_c/marker"), "x").unwrap();

        clone_prefix(&src, &dst, &|_| {}).unwrap();
        assert_eq!(fs::read_to_string(dst.join("pfx/drive_c/marker")).unwrap(), "x");
        assert!(dst.join("config_info").is_file());

        fs::write(dst.join("keep"), "x").unwrap();
        clone_prefix(&src, &dst, &|_| {}).unwrap();
        assert!(dst.join("keep").is_file(), "existing prefix must not be overwritten");

        fs::remove_dir_all(root).ok();
    }
}
