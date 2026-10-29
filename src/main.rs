//! Second Extinction launcher for a custom backend (Linux / Proton).
//!
//! Steps: find the game, patch the exe (Steam matchmaking instead of PlayFab Party),
//! trust the server's CA and route WinHTTP through its proxy in the Proton prefix,
//! then start the game through Steam with the Apex Connect redirect flags.

use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::{env, fs};

const APP_ID: &str = "1024380";
const EXE: &str = "SecondExtinction_F.exe";
/// sha256 of the build the patch offsets were taken from.
const EXE_SHA256: &str = "1a80f8b3168be1ba24e5f1d47da959c5ddbe54e4e6cc76ea7f54e31bfc483743";
/// (file offset, original, patched): crossplay flag default (VA 0x1434a6aa0) 1 -> 0, which makes
/// the backend choice at 0x140bf34f5 build the Steam matchmaking backend instead of PlayFab Party.
const PATCHES: &[(usize, u8, u8)] = &[(0x34A4AA0, 1, 0)];

const CERT_KEY: &str = r"HKEY_LOCAL_MACHINE\Software\Microsoft\SystemCertificates\Root\Certificates";
const CONN_KEY: &str =
    r"HKEY_LOCAL_MACHINE\Software\Microsoft\Windows\CurrentVersion\Internet Settings\Connections";

struct Opts {
    server: Option<String>,
    social_port: u16,
    proxy_port: u16,
    revert: bool,
    no_launch: bool,
    game_dir: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "usage: se-launcher --server HOST [--social-port 8443] [--proxy-port 8888]\n\
         \x20                  [--game-dir DIR] [--no-launch]\n\
         \x20      se-launcher --revert [--game-dir DIR]"
    );
    exit(2)
}

fn parse_args() -> Opts {
    let mut o = Opts { server: None, social_port: 8443, proxy_port: 8888, revert: false, no_launch: false, game_dir: None };
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--server" => o.server = Some(val()),
            "--social-port" => o.social_port = val().parse().unwrap_or_else(|_| usage()),
            "--proxy-port" => o.proxy_port = val().parse().unwrap_or_else(|_| usage()),
            "--game-dir" => o.game_dir = Some(PathBuf::from(val())),
            "--revert" => o.revert = true,
            "--no-launch" => o.no_launch = true,
            _ => usage(),
        }
    }
    if !o.revert && o.server.is_none() {
        usage()
    }
    o
}

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("error: {}", msg.as_ref());
    exit(1)
}

fn steam_root() -> PathBuf {
    let home = PathBuf::from(env::var("HOME").unwrap_or_else(|_| die("HOME not set")));
    [".local/share/Steam", ".steam/steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"]
        .iter()
        .map(|p| home.join(p))
        .find(|p| p.join("steamapps").is_dir())
        .unwrap_or_else(|| die("Steam installation not found"))
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

/// (game dir, library dir)
fn find_game(o: &Opts) -> (PathBuf, PathBuf) {
    if let Some(d) = &o.game_dir {
        let lib = d.ancestors().nth(3).unwrap_or_else(|| die("cannot derive library from --game-dir"));
        return (d.clone(), lib.to_path_buf());
    }
    libraries(&steam_root())
        .into_iter()
        .map(|l| (l.join("steamapps/common/SecondExtinction"), l))
        .find(|(g, _)| g.join(EXE).is_file())
        .unwrap_or_else(|| die("Second Extinction not found in any Steam library; use --game-dir"))
}

fn game_running() -> bool {
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

fn patch_exe(game: &Path) {
    let exe = game.join(EXE);
    let backup = game.join(format!("{EXE}.orig"));
    let mut data = fs::read(&exe).unwrap_or_else(|e| die(format!("read {}: {e}", exe.display())));
    if !backup.exists() {
        if hex(&Sha256::digest(&data)) != EXE_SHA256 {
            die("exe does not match the analyzed build (game updated?); refusing to patch");
        }
        fs::copy(&exe, &backup).unwrap_or_else(|e| die(format!("backup: {e}")));
        println!("backup   {}", backup.display());
    }
    for &(off, old, new) in PATCHES {
        match data[off] {
            b if b == new => println!("patch    {off:#x} already applied"),
            b if b == old => {
                data[off] = new;
                println!("patch    {off:#x}: {old:#04x} -> {new:#04x} (Steam matchmaking)");
            }
            b => die(format!("unexpected byte {b:#04x} at {off:#x}")),
        }
    }
    fs::write(&exe, data).unwrap_or_else(|e| die(format!("write exe: {e}")));
}

fn revert_exe(game: &Path) {
    let backup = game.join(format!("{EXE}.orig"));
    if backup.exists() {
        fs::copy(&backup, game.join(EXE)).unwrap_or_else(|e| die(format!("restore: {e}")));
        fs::remove_file(&backup).ok();
        println!("restored {EXE}");
    } else {
        println!("exe      no backup, nothing to restore");
    }
}

fn pem_to_der(pem: &str) -> Vec<u8> {
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    decode_b64(&b64).unwrap_or_else(|| die("server returned an invalid CA certificate"))
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
fn proton_wine(compat: &Path) -> PathBuf {
    let info = fs::read_to_string(compat.join("config_info"))
        .unwrap_or_else(|_| die("no config_info; start the game once through Steam first"));
    let fonts = info.lines().nth(1).unwrap_or_else(|| die("unexpected config_info format"));
    let files = Path::new(fonts.trim_end_matches('/')).ancestors().nth(2).unwrap_or_else(|| die("bad fonts path"));
    let wine = files.join("bin/wine");
    if !wine.is_file() {
        die(format!("Proton wine not found at {}", wine.display()));
    }
    wine
}

fn import_reg(compat: &Path, reg: &str) {
    let tmp = env::temp_dir().join("se-launcher.reg");
    let utf16: Vec<u8> = [0xFEFFu16].into_iter().chain(reg.encode_utf16()).flat_map(|c| c.to_le_bytes()).collect();
    fs::write(&tmp, utf16).unwrap_or_else(|e| die(format!("write reg: {e}")));
    let status = Command::new(proton_wine(compat))
        .env("WINEPREFIX", compat.join("pfx"))
        .env("WINEDEBUG", "-all")
        .args(["regedit", "/S"])
        .arg(format!("Z:{}", tmp.display()))
        .status()
        .unwrap_or_else(|e| die(format!("run wine: {e}")));
    fs::remove_file(&tmp).ok();
    if !status.success() {
        die("regedit failed");
    }
    // let wineserver flush the registry to disk
    Command::new(proton_wine(compat).with_file_name("wineserver"))
        .env("WINEPREFIX", compat.join("pfx"))
        .arg("-w")
        .status()
        .ok();
}

fn state_file(compat: &Path) -> PathBuf {
    compat.join("se-launcher-ca.sha1")
}

fn setup_prefix(compat: &Path, server: &str, o: &Opts) {
    let url = format!("http://{server}:{}/ca.pem", o.social_port);
    let pem = ureq::get(&url)
        .call()
        .unwrap_or_else(|e| die(format!("fetch {url}: {e}")))
        .into_string()
        .unwrap_or_else(|e| die(format!("read CA: {e}")));
    let der = pem_to_der(&pem);
    let thumb = hex(&Sha1::digest(&der)).to_uppercase();
    let proxy = format!("{server}:{}", o.proxy_port);
    // drop a CA from a previous server before trusting the new one
    if let Ok(old) = fs::read_to_string(state_file(compat)) {
        if old.trim() != thumb {
            import_reg(compat, &reg_file(None, old.trim(), Some(&proxy)));
        }
    }
    import_reg(compat, &reg_file(Some(&der), &thumb, Some(&proxy)));
    fs::write(state_file(compat), &thumb).ok();
    println!("prefix   trusts CA {thumb}, WinHTTP proxy {proxy}");
}

fn revert_prefix(compat: &Path) {
    let thumb = fs::read_to_string(state_file(compat)).unwrap_or_default();
    import_reg(compat, &reg_file(None, thumb.trim(), None));
    fs::remove_file(state_file(compat)).ok();
    println!("prefix   removed CA and WinHTTP proxy");
}

fn main() {
    let o = parse_args();
    if game_running() {
        die("Second Extinction is running; close it first");
    }
    let (game, lib) = find_game(&o);
    let compat = lib.join("steamapps/compatdata").join(APP_ID);
    println!("game     {}", game.display());

    if o.revert {
        revert_exe(&game);
        revert_prefix(&compat);
        return;
    }
    let server = o.server.as_deref().unwrap();
    patch_exe(&game);
    setup_prefix(&compat, server, &o);

    let args = ["--social-host", server, "--social-port", &o.social_port.to_string(), "--social-disable-ssl"];
    if o.no_launch {
        println!("launch   skipped; game arguments: {}", args.join(" "));
        return;
    }
    Command::new("steam")
        .args(["-applaunch", APP_ID])
        .args(args)
        .spawn()
        .unwrap_or_else(|e| die(format!("start steam: {e}")));
    println!("launch   steam -applaunch {APP_ID} {}", args.join(" "));
}
