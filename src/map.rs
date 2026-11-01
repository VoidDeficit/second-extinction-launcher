//! Builds the world map from the local game files and sends it to the server, so
//! the admin panel shows it without any game art being published. The server keeps
//! the first valid image, so this only runs the full build when it still needs one.
//! Same steps as tools/se_extract_map.py in the server repository.

use image::{RgbImage, imageops};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

const TILE_SIZE: u32 = 699_256;
const HEADER: usize = 192;
const BC1_TOP_MIP: usize = 524_288;
const TILE_RUN: usize = 1365;
const LEVEL3_START: usize = 21;
const WIDTH: u32 = 8;
const CELL: u32 = 384;
const PROBE: u32 = 32;
/// Must match MAP_SIZE and the crop in the server's src/live.rs.
const CROP: (u32, u32, u32, u32) = (592, 1147, 1226, 1413);

fn u16le(b: &[u8], i: usize) -> u32 {
    u32::from(b[i]) | u32::from(b[i + 1]) << 8
}

fn u32le(b: &[u8], i: usize) -> u32 {
    u16le(b, i) | u16le(b, i + 2) << 16
}

/// The compressed tile entries (offset, size), in archive order.
fn tile_run(tab: &[u8]) -> Option<Vec<(u64, usize)>> {
    let mut entries = Vec::new();
    let mut prev = u64::MAX;
    let mut pos = tab.len().checked_sub(24)?;
    while pos >= 0x18 {
        let h2 = u64::from(u32le(tab, pos + 4));
        if h2 > prev {
            break;
        }
        prev = h2;
        if tab[pos + 22] == 3 && tab[pos + 23] == 1 && u32le(tab, pos + 16) == TILE_SIZE {
            entries.push((u64::from(u32le(tab, pos + 8)), u32le(tab, pos + 12) as usize));
        }
        pos -= 24;
    }
    entries.sort_unstable();
    let mut start = 0;
    for i in 1..=entries.len() {
        let gap = i == entries.len() || entries[i].0 - (entries[i - 1].0 + entries[i - 1].1 as u64) > 1 << 20;
        if gap {
            if i - start == TILE_RUN {
                return Some(entries[start..i].to_vec());
            }
            start = i;
        }
    }
    None
}

fn rgb565(c: u32) -> [i32; 3] {
    [((c >> 11 & 31) * 255 / 31) as i32, ((c >> 5 & 63) * 255 / 63) as i32, ((c & 31) * 255 / 31) as i32]
}

fn decode_bc1(data: &[u8], size: u32) -> RgbImage {
    let mut img = RgbImage::new(size, size);
    for (n, block) in data.chunks_exact(8).enumerate() {
        let (bx, by) = (n as u32 % (size / 4) * 4, n as u32 / (size / 4) * 4);
        let (c0, c1) = (u16le(block, 0), u16le(block, 2));
        let (a, b) = (rgb565(c0), rgb565(c1));
        let mix = |p: i32, q: i32, r: i32, d: i32| [(a[0] * p + b[0] * q) / d + r, (a[1] * p + b[1] * q) / d + r, (a[2] * p + b[2] * q) / d + r];
        let palette = if c0 > c1 { [a, b, mix(2, 1, 0, 3), mix(1, 2, 0, 3)] } else { [a, b, mix(1, 1, 0, 2), [0, 0, 0]] };
        let bits = u32le(block, 4);
        for i in 0..16 {
            let c = palette[(bits >> (i * 2) & 3) as usize];
            img.put_pixel(bx + i % 4, by + i / 4, image::Rgb([c[0] as u8, c[1] as u8, c[2] as u8]));
        }
    }
    img
}

fn signature(img: &RgbImage) -> Vec<f32> {
    img.pixels().flat_map(|p| p.0).map(|v| f32::from(v).sqrt()).collect()
}

/// Level 3 tiles aren't stored in reading order: each goes where it best matches
/// the one-tile overview, which also fills any cell no tile fits.
pub fn build(game: &Path) -> Result<Vec<u8>, String> {
    let base = game.join("archives_win64/initial/game2");
    let tab = std::fs::read(base.with_extension("tab")).map_err(|e| format!("read map index: {e}"))?;
    let run = tile_run(&tab).ok_or("map tiles not found in the game files")?;
    let mut arc = File::open(base.with_extension("arc")).map_err(|e| format!("open map archive: {e}"))?;
    let mut tile = |i: usize| -> Result<RgbImage, String> {
        let (off, size) = run[i];
        let mut packed = vec![0; size];
        arc.seek(SeekFrom::Start(off)).and_then(|_| arc.read_exact(&mut packed)).map_err(|e| format!("read map tile: {e}"))?;
        let mut raw = Vec::new();
        // a tile can be stored as several zstd frames
        let mut rest = packed.as_slice();
        while !rest.is_empty() {
            ruzstd::decoding::StreamingDecoder::new(&mut rest)
                .map_err(|e| format!("map tile: {e}"))?
                .read_to_end(&mut raw)
                .map_err(|e| format!("map tile: {e}"))?;
        }
        if !raw.starts_with(b"AVTX") || raw.len() < HEADER + BC1_TOP_MIP {
            return Err("unexpected map tile data".into());
        }
        Ok(decode_bc1(&raw[HEADER..HEADER + BC1_TOP_MIP], 1024))
    };

    let overview = tile(0)?;
    let side = WIDTH * CELL;
    let mut canvas = imageops::resize(&overview, side, side, imageops::FilterType::CatmullRom);
    let grid = imageops::resize(&overview, WIDTH * PROBE, WIDTH * PROBE, imageops::FilterType::Triangle);
    let cells: Vec<Vec<f32>> = (0..WIDTH * WIDTH)
        .map(|c| signature(&imageops::crop_imm(&grid, c % WIDTH * PROBE, c / WIDTH * PROBE, PROBE, PROBE).to_image()))
        .collect();
    let mut tiles = Vec::new();
    for i in 0..(WIDTH * WIDTH) as usize {
        tiles.push(Some(imageops::resize(&tile(LEVEL3_START + i)?, CELL, CELL, imageops::FilterType::Lanczos3)));
    }
    let mut ranked = Vec::new();
    for (t, img) in tiles.iter().enumerate() {
        let probe = signature(&imageops::resize(img.as_ref().unwrap(), PROBE, PROBE, imageops::FilterType::Triangle));
        for (c, cell) in cells.iter().enumerate() {
            let err = probe.iter().zip(cell).map(|(a, b)| (a - b).abs()).sum::<f32>() / probe.len() as f32;
            if err < 1.0 {
                ranked.push((err, t, c));
            }
        }
    }
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut free = vec![true; cells.len()];
    for (_, t, c) in ranked {
        if free[c] {
            if let Some(img) = tiles[t].take() {
                free[c] = false;
                imageops::replace(&mut canvas, &img, i64::from(c as u32 % WIDTH * CELL), i64::from(c as u32 / WIDTH * CELL));
            }
        }
    }
    let map = imageops::crop_imm(&canvas, CROP.0, CROP.1, CROP.2, CROP.3).to_image();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85).encode_image(&map).map_err(|e| format!("encode map: {e}"))?;
    Ok(jpeg)
}

fn connect(server: &str, port: u16) -> Result<BufReader<TcpStream>, String> {
    let addr = (server, port).to_socket_addrs().ok().and_then(|mut a| a.next()).ok_or("cannot resolve the server")?;
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(120))).ok();
    Ok(BufReader::new(s))
}

fn reply(s: &mut BufReader<TcpStream>) -> Result<String, String> {
    let mut line = String::new();
    s.read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim_end().to_string())
}

fn sync(game: &Path, server: &str, port: u16) -> Result<bool, String> {
    let mut q = connect(server, port)?;
    q.get_mut().write_all(b"MAPQUERY\n").map_err(|e| e.to_string())?;
    if reply(&mut q)? != "NEED" {
        return Ok(false);
    }
    let jpeg = build(game)?;
    let mut s = connect(server, port)?;
    s.get_mut().write_all(format!("MAP {}\n", jpeg.len()).as_bytes()).and_then(|()| s.get_mut().write_all(&jpeg)).map_err(|e| e.to_string())?;
    match reply(&mut s)?.as_str() {
        "OK" => Ok(true),
        other => Err(format!("server answered {other:?}")),
    }
}

/// Runs in the background; the game starts normally if the map can't be sent.
pub fn sync_in_background(game: &Path, server: &str, port: u16) {
    let (game, server) = (game.to_path_buf(), server.to_string());
    std::thread::spawn(move || {
        if let Err(e) = sync(&game, &server, port) {
            eprintln!("world map not sent: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bc1_block_decodes_endpoints() {
        let mut data = vec![0u8; 8];
        data[0..2].copy_from_slice(&0xF800u16.to_le_bytes());
        data[2..4].copy_from_slice(&0x001Fu16.to_le_bytes());
        data[4] = 0b01; // first pixel uses the second endpoint
        let img = decode_bc1(&data, 4);
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0]);
    }

    /// Needs a local install: SE_GAME_DIR=... cargo test --release real_map -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_map_has_the_expected_size() {
        let jpeg = build(Path::new(&std::env::var("SE_GAME_DIR").unwrap())).unwrap();
        let img = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (CROP.2, CROP.3));
        std::fs::write(std::env::var("SE_MAP_OUT").unwrap(), &jpeg).unwrap();
    }
}
