//! Game discovery, exe patching, Proton prefix setup and launch.
//!
//! Steps: find the game, patch the exe (Steam matchmaking instead of PlayFab Party),
//! trust the server's CA and route WinHTTP through its proxy in the Proton prefix,
//! then start the game through Steam with the Apex Connect redirect flags.

use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

pub type Result<T> = std::result::Result<T, String>;

pub const APP_ID: &str = "1024380";
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings { server: "127.0.0.1".into(), social_port: 8443, proxy_port: 8888, game_dir: None }
    }
}

/// Where the game and its Proton prefix live.
pub struct Game {
    pub dir: PathBuf,
    pub compat: PathBuf,
}

/// What is currently applied, for the UI status line.
pub struct Status {
    pub patched: bool,
    pub ca_thumbprint: Option<String>,
}

fn steam_root() -> Result<PathBuf> {
    let home = PathBuf::from(env::var("HOME").map_err(|_| "HOME not set")?);
    [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"]
        .iter()
        .map(|p| home.join(p))
        .find(|p| p.join("steamapps").is_dir())
        .ok_or_else(|| "Steam installation not found".into())
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
    for &(off, old, new) in PATCHES {
        match data[off] {
            b if b == new => log(format!("Patch at {off:#x} already applied")),
            b if b == old => {
                data[off] = new;
                log(format!("Patched {off:#x}: {old:#04x} -> {new:#04x} (Steam matchmaking)"));
            }
            b => return Err(format!("unexpected byte {b:#04x} at {off:#x}")),
        }
    }
    fs::write(&exe, data).map_err(|e| format!("write exe: {e}"))
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

/// Proton's wine binary for this prefix, from compatdata/<app>/config_info (line 2 = fonts dir).
fn proton_wine(compat: &Path) -> Result<PathBuf> {
    let info = fs::read_to_string(compat.join("config_info"))
        .map_err(|_| "no Proton prefix yet; start the game once through Steam first")?;
    let fonts = info.lines().nth(1).ok_or("unexpected config_info format")?;
    let files = Path::new(fonts.trim_end_matches('/')).ancestors().nth(2).ok_or("bad fonts path")?;
    let wine = files.join("bin/wine");
    if !wine.is_file() {
        return Err(format!("Proton wine not found at {}", wine.display()));
    }
    Ok(wine)
}

fn import_reg(compat: &Path, reg: &str) -> Result<()> {
    let wine = proton_wine(compat)?;
    let tmp = env::temp_dir().join(format!("se-launcher-{}.reg", std::process::id()));
    let utf16: Vec<u8> = [0xFEFFu16].into_iter().chain(reg.encode_utf16()).flat_map(|c| c.to_le_bytes()).collect();
    fs::write(&tmp, utf16).map_err(|e| format!("write reg: {e}"))?;
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

fn state_file(compat: &Path) -> PathBuf {
    compat.join("se-launcher-ca.sha1")
}

pub fn setup_prefix(g: &Game, s: &Settings, log: &dyn Fn(String)) -> Result<()> {
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
    // drop a CA from a previous server before trusting the new one
    if let Ok(old) = fs::read_to_string(state_file(&g.compat)) {
        if old.trim() != thumb {
            log(format!("Removing previous server CA {}", old.trim()));
            import_reg(&g.compat, &reg_file(None, old.trim(), Some(&proxy)))?;
        }
    }
    log("Updating Proton prefix (this can take a few seconds)".into());
    import_reg(&g.compat, &reg_file(Some(&der), &thumb, Some(&proxy)))?;
    fs::write(state_file(&g.compat), &thumb).ok();
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
    ["--social-host", &s.server, "--social-port", &s.social_port.to_string(), "--social-disable-ssl"]
        .iter()
        .map(|a| a.to_string())
        .collect()
}

pub fn launch(s: &Settings, log: &dyn Fn(String)) -> Result<()> {
    let args = launch_args(s);
    Command::new("steam")
        .args(["-applaunch", APP_ID])
        .args(&args)
        .spawn()
        .map_err(|e| format!("start steam: {e}"))?;
    log(format!("Started: steam -applaunch {APP_ID} {}", args.join(" ")));
    Ok(())
}

/// Patch, set up the prefix and start the game.
pub fn play(s: &Settings, log: &dyn Fn(String)) -> Result<()> {
    if game_running() {
        return Err("Second Extinction is already running; close it first".into());
    }
    let g = find_game(s)?;
    log(format!("Game: {}", g.dir.display()));
    patch_exe(&g, log)?;
    setup_prefix(&g, s, log)?;
    launch(s, log)
}

/// Undo the exe patch and prefix changes.
pub fn revert(s: &Settings, log: &dyn Fn(String)) -> Result<()> {
    if game_running() {
        return Err("Second Extinction is running; close it first".into());
    }
    let g = find_game(s)?;
    revert_exe(&g, log)?;
    revert_prefix(&g, log)
}
