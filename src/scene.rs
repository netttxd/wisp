//! Plays Wallpaper Engine scenes without Wallpaper Engine (prototype).
//!
//! A scene is a recipe: scene.json lists image layers, each with a chain of effects; every effect is a few
//! shader passes. This reads scene.pkg, decodes its .tex images, turns Wallpaper Engine's shader dialect
//! into plain OpenGL GLSL and runs the passes. Built-in pieces (common.h, util textures) come from the
//! Wallpaper Engine install, which owning a Scene implies. Not yet: particles, puppets, 3D models, scripts.

use super::*;
use eframe::{
    egui, egui_glow,
    glow::{self, HasContext},
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

// ---------- Files ----------

/// The files of one scene: what its scene.pkg holds, then its folder, then Wallpaper Engine's assets.
pub struct Assets {
    /// Each file in scene.pkg: where it starts in the package, and its bytes.
    files: HashMap<String, (usize, Vec<u8>)>,
    dirs: Vec<PathBuf>,
}

/// Unpacks scene.pkg: a version string, a file table (name, offset, size), then the file bytes.
fn read_pkg(b: &[u8]) -> Option<HashMap<String, (usize, Vec<u8>)>> {
    let (table, start) = pkg_table(b)?;
    table.into_iter().map(|(name, offset, size)| Some((name, (start + offset, b.get(start + offset..start + offset + size)?.to_vec())))).collect()
}

/// scene.pkg's file table (name, offset from the end of the table, size), and where the table ends.
fn pkg_table(b: &[u8]) -> Option<(Vec<(String, usize, usize)>, usize)> {
    let mut pos = 0usize;
    let int = |pos: &mut usize| -> Option<usize> {
        let v = u32::from_le_bytes(b.get(*pos..*pos + 4)?.try_into().ok()?) as usize;
        *pos += 4;
        Some(v)
    };
    let string = |pos: &mut usize| -> Option<String> {
        let n = int(pos)?;
        let s = String::from_utf8_lossy(b.get(*pos..*pos + n)?).into_owned();
        *pos += n;
        Some(s)
    };
    let _version = string(&mut pos)?;
    let count = int(&mut pos)?;
    let mut table = Vec::with_capacity(count.min(10_000));
    for _ in 0..count {
        let name = string(&mut pos)?;
        let (offset, size) = (int(&mut pos)?, int(&mut pos)?);
        table.push((name, offset, size));
    }
    Some((table, pos))
}

/// A scene's shape (width / height) from its scene.json, reading only the package's table and that file, so the
/// window can show how it fits without loading the whole scene.
pub fn scene_aspect(item: &Path) -> Option<f32> {
    use std::io::{Read, Seek, SeekFrom};
    let json = match fs::read(item.join("scene.json")) {
        Ok(b) => b,
        Err(_) => {
            let mut f = fs::File::open(item.join("scene.pkg")).ok()?;
            // The table comes first and is far smaller than this.
            let mut head = Vec::new();
            (&mut f).take(1 << 20).read_to_end(&mut head).ok()?;
            let (table, start) = pkg_table(&head)?;
            let (_, offset, size) = table.into_iter().find(|(n, _, _)| n == "scene.json")?;
            f.seek(SeekFrom::Start((start + offset) as u64)).ok()?;
            let mut b = vec![0; size.min(64 << 20)];
            f.read_exact(&mut b).ok()?;
            b
        }
    };
    let v: Value = serde_json::from_slice(&json).ok()?;
    let o = &v["general"]["orthogonalprojection"];
    // As Scene::load: no size given means 1920 x 1080.
    let (w, h) = (o["width"].as_f64().unwrap_or(1920.0), o["height"].as_f64().unwrap_or(1080.0));
    (w > 0.0 && h > 0.0).then(|| (w / h) as f32)
}

/// Wallpaper Engine's built-in assets folder, if it is installed.
fn we_assets() -> Option<PathBuf> {
    steam_libraries().into_iter().map(|lib| lib.join(r"steamapps\common\wallpaper_engine\assets")).find(|p| p.exists())
}

impl Assets {
    pub fn open(item: &Path) -> Option<Assets> {
        let files = read_pkg(&fs::read(item.join("scene.pkg")).ok()?)?;
        let mut dirs = vec![item.to_path_buf()];
        dirs.extend(we_assets());
        Some(Assets { files, dirs })
    }

    fn get(&self, name: &str) -> Option<Vec<u8>> {
        let name = name.replace('\\', "/");
        self.files.get(&name).map(|(_, b)| b.clone()).or_else(|| self.dirs.iter().find_map(|d| fs::read(d.join(&name)).ok()))
    }

    /// Where mpv can play a file from: inside scene.pkg as a byte range (mpv's slice:// reads just that part,
    /// nothing is unpacked to disk), or the file itself when it sits next to the package.
    fn media_url(&self, name: &str) -> Option<String> {
        let name = name.replace('\\', "/");
        if let Some((start, bytes)) = self.files.get(&name) {
            return Some(format!("slice://{start}-{}@{}", start + bytes.len(), self.dirs[0].join("scene.pkg").display()));
        }
        self.dirs.iter().map(|d| d.join(&name)).find(|p| p.is_file()).map(|p| p.display().to_string())
    }

    fn text(&self, name: &str) -> Option<String> {
        self.get(name).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    fn json(&self, name: &str) -> Option<Value> {
        serde_json::from_slice(&self.get(name)?).ok()
    }
}

// ---------- Textures ----------

/// A decoded texture: pixels as RGBA, the image's own size inside a possibly padded texture, and for
/// sprite sheets the frames as (x, y, width, height) in 0..1 texture coordinates.
pub struct Tex {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
    img_w: u32,
    img_h: u32,
    frames: Vec<[f32; 4]>,
    /// 1 or 2 for one- or two-channel textures (masks, particle sprites), 4 for color.
    channels: u8,
}

/// Decodes Wallpaper Engine's .tex: a header, images made of mipmaps that are either an embedded JPEG/PNG
/// or raw pixels (often LZ4-compressed), and for sprite sheets a frame table. Only the full-size image is used.
/// `max_px`: the widest the screen draws the scene; bigger embedded pictures are shrunk to it (at least 2048).
pub fn decode_tex(b: &[u8], max_px: u32) -> Result<Tex, String> {
    let mut pos = 0usize;
    let bytes4 = |pos: &mut usize| -> Result<[u8; 4], String> {
        let v: [u8; 4] = b.get(*pos..*pos + 4).ok_or("tex: cut short")?.try_into().unwrap();
        *pos += 4;
        Ok(v)
    };
    let int = |pos: &mut usize| bytes4(pos).map(i32::from_le_bytes);
    let float = |pos: &mut usize| bytes4(pos).map(f32::from_le_bytes);
    let tag = |pos: &mut usize| -> Result<String, String> {
        let t = String::from_utf8_lossy(b.get(*pos..*pos + 8).ok_or("tex: cut short")?).into_owned();
        *pos += 9; // 8 letters and a zero
        Ok(t)
    };
    if tag(&mut pos)? != "TEXV0005" || tag(&mut pos)? != "TEXI0001" {
        return Err("tex: unknown header".into());
    }
    let format = int(&mut pos)?;
    let flags = int(&mut pos)?;
    let (_tw, _th, img_w, img_h) = (int(&mut pos)?, int(&mut pos)?, int(&mut pos)?, int(&mut pos)?);
    let _ = int(&mut pos)?;
    let container = tag(&mut pos)?;
    let images = int(&mut pos)?;
    let embedded = if container.as_str() >= "TEXB0003" { int(&mut pos)? } else { -1 };
    if container.as_str() >= "TEXB0004" && int(&mut pos)? == 1 {
        return Err("tex: video texture, not supported yet".into());
    }
    // Walk every image and mipmap (the frame table comes after them all), keeping the first full-size one.
    let mut first: Option<(u32, u32, bool, usize, &[u8])> = None;
    for image in 0..images.max(0) {
        let mips = int(&mut pos)?;
        for mip in 0..mips.max(0) {
            let (w, h) = (int(&mut pos)? as u32, int(&mut pos)? as u32);
            let (mut lz4, mut full) = (0, 0);
            if container.as_str() >= "TEXB0002" {
                lz4 = int(&mut pos)?;
                full = int(&mut pos)?;
            }
            let size = int(&mut pos)? as usize;
            let data = b.get(pos..pos + size).ok_or("tex: cut short")?;
            pos += size;
            if image == 0 && mip == 0 {
                first = Some((w, h, lz4 == 1, full as usize, data));
            }
        }
    }
    let (w, h, lz4, full, data) = first.ok_or("tex: no image")?;
    let data = if lz4 { lz4_flex::block::decompress(data, full).map_err(|e| format!("tex lz4: {e}"))? } else { data.to_vec() };
    let (w, h, rgba) = if embedded != -1 {
        let mut img = image::load_from_memory(&data).map_err(|e| format!("tex image: {e}"))?.to_rgba8();
        // Pictures wider than the screen can show are shrunk to save memory, with a sharp filter (CatmullRom):
        // the softer Triangle filter blurred fine lines in anime art.
        let cap = max_px.max(2048);
        if img.width() > cap && flags & 4 == 0 {
            let h = (img.height() as u64 * cap as u64 / img.width() as u64) as u32;
            img = image::imageops::resize(&img, cap, h, image::imageops::FilterType::CatmullRom);
        }
        let (w, h) = img.dimensions();
        (w, h, img.into_raw())
    } else {
        let px = (w * h) as usize;
        // Same layout OpenGL gives single and dual channel textures: missing channels 0, alpha 1.
        let rgba = match format {
            0 => data.get(..px * 4).ok_or("tex: short RGBA")?.to_vec(),
            8 => data.get(..px * 2).ok_or("tex: short RG")?.chunks(2).flat_map(|c| [c[0], c[1], 0, 255]).collect(),
            9 => data.get(..px).ok_or("tex: short R")?.iter().flat_map(|&r| [r, 0, 0, 255]).collect(),
            4 => decode_dxt(&data, w, h, 5)?,
            6 => decode_dxt(&data, w, h, 3)?,
            7 => decode_dxt(&data, w, h, 1)?,
            f => return Err(format!("tex: format {f} not supported yet")),
        };
        (w, h, rgba)
    };
    // Sprite sheets: a frame table of pixel rectangles (whole numbers in version 1, floats after).
    let mut frames = Vec::new();
    if flags & 4 != 0 {
        let version = tag(&mut pos)?;
        let count = int(&mut pos)?;
        if version.as_str() >= "TEXS0003" {
            let _ = (int(&mut pos)?, int(&mut pos)?);
        }
        let (fw, fh) = (img_w.max(1) as f32, img_h.max(1) as f32);
        for _ in 0..count.clamp(0, 4096) {
            let _image = int(&mut pos)?;
            let _time = float(&mut pos)?;
            let mut v = [0f32; 6];
            for x in v.iter_mut() {
                *x = if version == "TEXS0001" { int(&mut pos)? as f32 } else { float(&mut pos)? };
            }
            // x, y, width, (skew), (skew), height
            frames.push([v[0] / fw, v[1] / fh, v[2] / fw, v[5] / fh]);
        }
    }
    let (img_w, img_h) = if embedded != -1 { (w, h) } else { (img_w as u32, img_h as u32) };
    let channels = match (embedded, format) {
        (-1, 9) => 1,
        (-1, 8) => 2,
        _ => 4,
    };
    Ok(Tex { w, h, rgba, img_w, img_h, frames, channels })
}

/// Unpacks DXT1/3/5 (BC1/2/3) compressed pixels: 4x4 blocks with two key colors and per-pixel picks.
fn decode_dxt(data: &[u8], w: u32, h: u32, kind: u8) -> Result<Vec<u8>, String> {
    let block = if kind == 1 { 8 } else { 16 };
    let (bw, bh) = (w.div_ceil(4) as usize, h.div_ceil(4) as usize);
    if data.len() < bw * bh * block {
        return Err("tex: short DXT data".into());
    }
    let mut out = vec![0u8; (w * h * 4) as usize];
    let rgb565 = |c: u16| [((c >> 11) & 31) as u32 * 255 / 31, ((c >> 5) & 63) as u32 * 255 / 63, (c & 31) as u32 * 255 / 31];
    for by in 0..bh {
        for bx in 0..bw {
            let b = &data[(by * bw + bx) * block..][..block];
            let (alpha, color) = if kind == 1 { (&b[0..0], &b[0..8]) } else { (&b[0..8], &b[8..16]) };
            let c0 = u16::from_le_bytes([color[0], color[1]]);
            let c1 = u16::from_le_bytes([color[2], color[3]]);
            let (p0, p1) = (rgb565(c0), rgb565(c1));
            let mix = |a: u32, b: u32, wa: u32, wb: u32| (a * wa + b * wb) / (wa + wb);
            let palette: [[u32; 4]; 4] = if c0 > c1 || kind != 1 {
                [[p0[0], p0[1], p0[2], 255], [p1[0], p1[1], p1[2], 255], [mix(p0[0], p1[0], 2, 1), mix(p0[1], p1[1], 2, 1), mix(p0[2], p1[2], 2, 1), 255], [mix(p0[0], p1[0], 1, 2), mix(p0[1], p1[1], 1, 2), mix(p0[2], p1[2], 1, 2), 255]]
            } else {
                [[p0[0], p0[1], p0[2], 255], [p1[0], p1[1], p1[2], 255], [mix(p0[0], p1[0], 1, 1), mix(p0[1], p1[1], 1, 1), mix(p0[2], p1[2], 1, 1), 255], [0, 0, 0, 0]]
            };
            let picks = u32::from_le_bytes([color[4], color[5], color[6], color[7]]);
            // DXT5 alpha: two key alphas and 3-bit picks; DXT3 alpha: 4 bits per pixel.
            let alphas: [u32; 8] = if kind == 5 {
                let (a0, a1) = (alpha[0] as u32, alpha[1] as u32);
                if a0 > a1 {
                    [a0, a1, (6 * a0 + a1) / 7, (5 * a0 + 2 * a1) / 7, (4 * a0 + 3 * a1) / 7, (3 * a0 + 4 * a1) / 7, (2 * a0 + 5 * a1) / 7, (a0 + 6 * a1) / 7]
                } else {
                    [a0, a1, (4 * a0 + a1) / 5, (3 * a0 + 2 * a1) / 5, (2 * a0 + 3 * a1) / 5, (a0 + 4 * a1) / 5, 0, 255]
                }
            } else {
                [0; 8]
            };
            let alpha_bits = if kind == 5 { alpha[2..8].iter().rev().fold(0u64, |acc, &x| (acc << 8) | x as u64) } else { 0 };
            for py in 0..4 {
                for px in 0..4 {
                    let (x, y) = (bx * 4 + px, by * 4 + py);
                    if x >= w as usize || y >= h as usize {
                        continue;
                    }
                    let i = py * 4 + px;
                    let mut c = palette[((picks >> (2 * i)) & 3) as usize];
                    if kind == 5 {
                        c[3] = alphas[((alpha_bits >> (3 * i)) & 7) as usize];
                    } else if kind == 3 {
                        c[3] = ((alpha[i / 2] >> (4 * (i % 2))) & 15) as u32 * 17;
                    }
                    let o = (y * w as usize + x) * 4;
                    out[o..o + 4].copy_from_slice(&[c[0] as u8, c[1] as u8, c[2] as u8, c[3] as u8]);
                }
            }
        }
    }
    Ok(out)
}

// ---------- Values ----------

/// Numbers from a scene value: "1 2 3", 0.5, or {"value": ...} for values a user can change.
fn floats(v: &Value) -> Vec<f32> {
    match v {
        Value::Number(n) => vec![n.as_f64().unwrap_or(0.0) as f32],
        Value::String(s) => s.split_whitespace().filter_map(|x| x.parse().ok()).collect(),
        Value::Bool(b) => vec![*b as i32 as f32],
        Value::Object(o) => o.get("value").map(floats).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(Value::Object(o)) => truthy(o.get("value")),
        Some(other) => floats(other).first().is_none_or(|x| *x != 0.0),
    }
}

// ---------- Shaders ----------

/// Wallpaper Engine writes HLSL-flavored GLSL 1.x; these turn its words into plain GLSL.
const PREAMBLE: &str = "#version 120
#define highp
#define mediump
#define lowp
#define mul(x, y) ((y) * (x))
#define frac fract
#define lerp mix
#define saturate(x) clamp(x, 0.0, 1.0)
#define CAST2(x) (vec2(x))
#define CAST3(x) (vec3(x))
#define CAST4(x) (vec4(x))
#define CAST3X3(x) (mat3(x))
#define texSample2D texture2D
#define texSample2DLod texture2DLod
#define atan2 atan
#define fmod mod
#define ddx dFdx
#define ddy(x) dFdy(-(x))
";

/// HLSL lets max/min/pow mix a number with a vector (`max(0, color)`); GLSL doesn't, and a user function named
/// max would hide all built-in ones. So shader calls are renamed to these helpers, which accept both styles.
fn helpers() -> String {
    let mut out = String::new();
    for f in ["max", "min", "pow"] {
        out += &format!("float w_{f}(float a, float b) {{ return {f}(a, b); }}\n");
        for t in ["vec2", "vec3", "vec4"] {
            out += &format!("{t} w_{f}({t} a, {t} b) {{ return {f}(a, b); }}\n");
            out += &format!("{t} w_{f}({t} a, float b) {{ return {f}(a, {t}(b)); }}\n");
            out += &format!("{t} w_{f}(float a, {t} b) {{ return {f}({t}(a), b); }}\n");
        }
    }
    out
}

/// Moves a shader's texture declarations above its includes: some shared includes (like common_blur.h) use
/// g_Texture0 before the shader declares it, which Wallpaper Engine's compiler allows and GLSL doesn't.
fn hoist_samplers(src: &str) -> String {
    // The space matters: "sampler2DComparison" (shadow maps) must stay inside its #if.
    let (samplers, rest): (Vec<&str>, Vec<&str>) = src.lines().partition(|l| l.trim_start().starts_with("uniform sampler2D "));
    let mut seen = HashSet::new();
    let mut out: String = samplers.into_iter().filter(|l| seen.insert(l.split_whitespace().nth(2).unwrap_or("").trim_end_matches(';').to_string())).map(|l| format!("{l}\n")).collect();
    out += &rest.join("\n");
    out
}

/// GLSL before version 1.40 has no inverse(); some effects use it.
const INVERSE: &str = "
mat2 inverse(mat2 m) { return mat2(m[1][1], -m[0][1], -m[1][0], m[0][0]) / (m[0][0] * m[1][1] - m[0][1] * m[1][0]); }
mat3 inverse(mat3 m) {
    float a00 = m[0][0], a01 = m[0][1], a02 = m[0][2], a10 = m[1][0], a11 = m[1][1], a12 = m[1][2], a20 = m[2][0], a21 = m[2][1], a22 = m[2][2];
    float b01 = a22 * a11 - a12 * a21, b11 = -a22 * a10 + a12 * a20, b21 = a21 * a10 - a11 * a20;
    return mat3(b01, -a22 * a01 + a02 * a21, a12 * a01 - a02 * a11, b11, a22 * a00 - a02 * a20, -a12 * a00 + a02 * a10, b21, -a21 * a00 + a01 * a20, a11 * a00 - a01 * a10) / (a00 * b01 + a01 * b11 + a02 * b21);
}
mat4 inverse(mat4 m) {
    float a00 = m[0][0], a01 = m[0][1], a02 = m[0][2], a03 = m[0][3], a10 = m[1][0], a11 = m[1][1], a12 = m[1][2], a13 = m[1][3];
    float a20 = m[2][0], a21 = m[2][1], a22 = m[2][2], a23 = m[2][3], a30 = m[3][0], a31 = m[3][1], a32 = m[3][2], a33 = m[3][3];
    float b00 = a00 * a11 - a01 * a10, b01 = a00 * a12 - a02 * a10, b02 = a00 * a13 - a03 * a10, b03 = a01 * a12 - a02 * a11;
    float b04 = a01 * a13 - a03 * a11, b05 = a02 * a13 - a03 * a12, b06 = a20 * a31 - a21 * a30, b07 = a20 * a32 - a22 * a30;
    float b08 = a20 * a33 - a23 * a30, b09 = a21 * a32 - a22 * a31, b10 = a21 * a33 - a23 * a31, b11 = a22 * a33 - a23 * a32;
    float det = b00 * b11 - b01 * b10 + b02 * b09 + b03 * b08 - b04 * b07 + b05 * b06;
    return mat4(a11 * b11 - a12 * b10 + a13 * b09, a02 * b10 - a01 * b11 - a03 * b09, a31 * b05 - a32 * b04 + a33 * b03, a22 * b04 - a21 * b05 - a23 * b03,
                a12 * b08 - a10 * b11 - a13 * b07, a00 * b11 - a02 * b08 + a03 * b07, a32 * b02 - a30 * b05 - a33 * b01, a20 * b05 - a22 * b02 + a23 * b01,
                a10 * b10 - a11 * b08 + a13 * b06, a01 * b08 - a00 * b10 - a03 * b06, a30 * b04 - a31 * b02 + a33 * b00, a21 * b02 - a20 * b04 - a23 * b00,
                a11 * b07 - a10 * b09 - a12 * b06, a00 * b09 - a01 * b07 + a02 * b06, a31 * b01 - a30 * b03 - a32 * b00, a20 * b03 - a21 * b01 + a22 * b00) / det;
}
";

/// Renames calls like `max(` to `w_max(`, only where max is a whole word.
fn rename_calls(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 64);
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let word_start = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'.');
        let hit = ["max", "min", "pow"].into_iter().find(|f| word_start && src[i..].starts_with(f) && src[i + f.len()..].trim_start().starts_with('('));
        match hit {
            Some(f) => {
                out += "w_";
                out += f;
                i += f.len();
            }
            None => {
                let c = src[i..].chars().next().unwrap();
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

/// One uniform a shader declares, with the JSON note after it (its material name, default, combo).
struct UniformInfo {
    ty: String,
    name: String,
    note: Value,
}

fn resolve_includes(src: &str, assets: &Assets, seen: &mut HashSet<String>) -> String {
    let mut out = String::new();
    for line in src.lines() {
        let t = line.trim();
        // "#require <feature>" is Wallpaper Engine's own note to itself; GLSL doesn't know it.
        if t.starts_with("#require") {
            out.push('\n');
            continue;
        }
        if let Some(rest) = t.strip_prefix("#include") {
            let name = rest.trim().trim_matches('"').to_string();
            if seen.insert(name.clone()) {
                let inc = assets.text(&format!("shaders/{name}")).unwrap_or_default();
                out += &resolve_includes(&inc, assets, seen);
            }
        } else {
            out += line;
        }
        out.push('\n');
    }
    out
}

fn uniforms(src: &str) -> Vec<UniformInfo> {
    src.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("uniform ")?;
            let (decl, note) = rest.split_once("//").map_or((rest, ""), |(d, n)| (d, n));
            let mut words = decl.trim().trim_end_matches(';').split_whitespace();
            let ty = words.next()?.to_string();
            let name = words.next()?.split('[').next()?.to_string();
            Some(UniformInfo { ty, name, note: serde_json::from_str(note.trim()).unwrap_or(Value::Null) })
        })
        .collect()
}

/// Combo switches a shader declares with `// [COMBO] {...}`, at their defaults.
fn combo_defaults(src: &str) -> HashMap<String, i64> {
    src.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim().strip_prefix("// [COMBO]")?.trim()).ok())
        .filter_map(|v| Some((v["combo"].as_str()?.to_string(), v["default"].as_i64().unwrap_or(0))))
        .collect()
}

/// Names tested with `#if X` but never with `#ifdef X`: those get `#define X 0` when unset, since some
/// drivers reject unknown names in `#if`. Names tested with #ifdef must stay undefined to stay off.
fn if_names(src: &str) -> HashSet<String> {
    let (mut tested, mut ifdef) = (HashSet::new(), HashSet::new());
    for l in src.lines().map(str::trim) {
        let (expr, set) = if let Some(e) = l.strip_prefix("#ifdef").or_else(|| l.strip_prefix("#ifndef")) {
            (e, &mut ifdef)
        } else if let Some(e) = l.strip_prefix("#if").or_else(|| l.strip_prefix("#elif")) {
            (e, &mut tested)
        } else {
            continue;
        };
        let expr = expr.split("//").next().unwrap_or("");
        for word in expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if word.chars().next().is_some_and(|c| c.is_ascii_uppercase() || c == '_') && word != "defined" {
                set.insert(word.to_string());
            }
        }
        if expr.contains("defined") {
            for word in expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
                ifdef.insert(word.to_string());
            }
        }
    }
    tested.retain(|n| !ifdef.contains(n));
    tested
}

/// Names the shader #defines itself. Defining them again from outside is a compile error.
fn own_defines(src: &str) -> HashSet<String> {
    src.lines().filter_map(|l| Some(l.trim().strip_prefix("#define")?.split_whitespace().next()?.split('(').next()?.to_string())).collect()
}

fn compile(gl: &glow::Context, vert: &str, frag: &str) -> Result<glow::Program, String> {
    unsafe {
        let program = gl.create_program()?;
        for (kind, src) in [(glow::VERTEX_SHADER, vert), (glow::FRAGMENT_SHADER, frag)] {
            let s = gl.create_shader(kind)?;
            gl.shader_source(s, src);
            gl.compile_shader(s);
            if !gl.get_shader_compile_status(s) {
                let log = gl.get_shader_info_log(s);
                gl.delete_shader(s);
                gl.delete_program(program);
                return Err(format!("{} shader: {log}", if kind == glow::VERTEX_SHADER { "vertex" } else { "fragment" }));
            }
            gl.attach_shader(program, s);
            gl.delete_shader(s);
        }
        gl.bind_attrib_location(program, 0, "a_Position");
        gl.bind_attrib_location(program, 1, "a_TexCoord");
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(format!("link: {log}"));
        }
        Ok(program)
    }
}

// ---------- Scene ----------

/// One shader pass: which program, its textures by slot, and the values it was given.
struct Pass {
    shader: String,
    combos: HashMap<String, i64>,
    textures: Vec<Option<String>>,
    values: HashMap<String, Value>,
    /// Render into this named buffer instead of the layer's next buffer.
    target: Option<String>,
    /// Extra buffers bound to texture slots.
    binds: Vec<(usize, String)>,
    /// Which of the layer's effects this pass belongs to: 0 for the layer's own picture, 1 for its first effect...
    effect: usize,
}

struct Layer {
    /// Center in scene coordinates (origin bottom left) and size after scaling.
    center: [f32; 2],
    size: [f32; 2],
    alpha: f32,
    color: [f32; 3],
    /// The base image pass, then every effect pass in order.
    passes: Vec<Pass>,
    /// Extra buffers its effects declare: name, size divisor.
    fbos: Vec<(String, f32)>,
    /// Place in the scene's object list: objects are drawn in this order.
    order: usize,
    /// Turn around the center, radians counterclockwise.
    angle: f32,
}

pub struct Scene {
    pub width: f32,
    pub height: f32,
    clear: [f32; 3],
    layers: Vec<Layer>,
    systems: Vec<System>,
    texts: Vec<TextDef>,
    /// Music and sound objects, played while wallpaper sound is on.
    sounds: Vec<SoundDef>,
    /// What this prototype skipped, to show in the preview.
    pub skipped: Vec<String>,
}

/// A sound object: its files (inside scene.pkg), loudness (1 is full) and how it repeats.
struct SoundDef {
    files: Vec<String>,
    volume: f32,
    /// "loop" (default), "random" (shuffled, repeating) or "single" (once).
    mode: String,
}

fn pass_from_material(assets: &Assets, material: &str, skipped: &mut Vec<String>) -> Option<Pass> {
    let m = assets.json(material).or_else(|| {
        skipped.push(format!("missing material {material}"));
        None
    })?;
    let p = &m["passes"][0];
    Some(Pass {
        shader: p["shader"].as_str()?.to_string(),
        combos: p["combos"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))).collect()).unwrap_or_default(),
        textures: p["textures"].as_array().map(|a| a.iter().map(|t| t.as_str().map(String::from)).collect()).unwrap_or_default(),
        values: p["constantshadervalues"].as_object().map(|o| o.clone().into_iter().collect()).unwrap_or_default(),
        target: None,
        binds: Vec::new(),
        effect: 0,
    })
}

// ---------- Scene settings ----------

/// A setting the scene offers in Wallpaper Engine's properties panel ("Toggle Clock", a color, a slider...).
pub struct UserProp {
    pub name: String,
    /// "bool", "slider", "color" or "combo".
    pub kind: String,
    pub label: String,
    /// The scene's default.
    pub value: Value,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    /// Combo choices: (label, value).
    pub options: Vec<(String, Value)>,
    /// Shown only while this bool setting is on (Wallpaper Engine's "clock.value" conditions).
    pub needs: Option<String>,
}

/// Every setting name scene.json uses.
fn used_settings(v: &Value, out: &mut HashSet<String>) {
    match v {
        Value::Object(o) => {
            let name = match o.get("user") {
                Some(Value::String(s)) => Some(s.as_str()),
                Some(Value::Object(u)) => u.get("name").and_then(Value::as_str),
                _ => None,
            };
            if let Some(n) = name {
                out.insert(n.to_string());
            }
            o.values().for_each(|x| used_settings(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| used_settings(x, out)),
        _ => {}
    }
}

/// The settings of a scene that change something Wisp draws, in the scene's order. Empty for a scene without any.
pub fn user_props(item: &Path) -> Vec<UserProp> {
    let Some(project) = fs::read(item.join("project.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return Vec::new() };
    let Some(scene) = fs::read(item.join("scene.pkg")).ok().and_then(|b| read_pkg(&b)).and_then(|f| serde_json::from_slice::<Value>(&f.get("scene.json")?.1).ok()) else { return Vec::new() };
    let mut used = HashSet::new();
    used_settings(&scene, &mut used);
    let mut props: Vec<(i64, UserProp)> = Vec::new();
    for (name, p) in project["general"]["properties"].as_object().into_iter().flatten() {
        let kind = p["type"].as_str().unwrap_or("");
        if !used.contains(name) || !["bool", "slider", "color", "combo"].contains(&kind) {
            continue;
        }
        // Labels are often HTML ("<b>Clock</b>") or Wallpaper Engine's own translation keys ("ui_...").
        let mut label = String::new();
        let mut in_tag = false;
        for ch in p["text"].as_str().unwrap_or("").chars() {
            match ch {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => label.push(c),
                _ => {}
            }
        }
        let label = label.replace("&amp;", "&").trim().to_string();
        let label = if label.is_empty() || label.starts_with("ui_") { name.replace('_', " ").trim().to_string() } else { label };
        let needs = p["condition"].as_str().and_then(|c| c.trim().strip_suffix(".value")).filter(|d| d.chars().all(|c| c.is_alphanumeric() || c == '_')).map(String::from);
        let options = p["options"].as_array().into_iter().flatten().map(|o| (o["label"].as_str().unwrap_or("?").to_string(), o["value"].clone())).collect();
        let num = |k: &str, d: f64| p[k].as_f64().or_else(|| p[k].as_str().and_then(|s| s.parse().ok())).unwrap_or(d);
        let max = num("max", 1.0);
        let step = num("step", if num("precision", 2.0) == 0.0 { 1.0 } else { 0.0 });
        let prop = UserProp { name: name.clone(), kind: kind.into(), label, value: p["value"].clone(), min: num("min", 0.0), max, step, options, needs };
        props.push((p["order"].as_i64().unwrap_or(0), prop));
    }
    props.sort_by_key(|(order, _)| *order);
    props.into_iter().map(|(_, p)| p).collect()
}

/// Puts the chosen settings into scene.json: each `{"user": name, "value": default}` becomes the setting's value.
/// A `{"user": {"name": n, "condition": c}}` becomes true when setting n equals c (a combo choice).
fn apply_settings(v: &mut Value, values: &serde_json::Map<String, Value>) {
    match v {
        Value::Object(o) => {
            let bound = match o.get("user") {
                Some(Value::String(s)) => Some((s.clone(), None)),
                Some(Value::Object(u)) => u.get("name").and_then(Value::as_str).map(|n| (n.to_string(), u.get("condition").map(|c| c.to_string().trim_matches('"').to_string()))),
                _ => None,
            };
            if let Some((name, condition)) = bound {
                *v = match (values.get(&name), condition) {
                    (Some(now), Some(c)) => Value::Bool(now.to_string().trim_matches('"') == c),
                    (Some(now), None) => now.clone(),
                    (None, _) => o.get("value").cloned().unwrap_or(Value::Null),
                };
                return;
            }
            o.values_mut().for_each(|x| apply_settings(x, values));
        }
        Value::Array(a) => a.iter_mut().for_each(|x| apply_settings(x, values)),
        _ => {}
    }
}

/// An object's place in the scene after applying its parents: center, scale, angle (radians, counterclockwise).
fn world_transform(obj: &Value, by_id: &HashMap<i64, &Value>, depth: u8) -> ([f32; 2], [f32; 2], f32) {
    let o = floats(&obj["origin"]);
    let s = floats(&obj["scale"]);
    let origin = [o.first().copied().unwrap_or(0.0), o.get(1).copied().unwrap_or(0.0)];
    let scale = [s.first().copied().unwrap_or(1.0), s.get(1).copied().unwrap_or(1.0)];
    let angle = floats(&obj["angles"]).get(2).copied().unwrap_or(0.0);
    match obj["parent"].as_i64().and_then(|p| by_id.get(&p)) {
        // A child's origin is in its parent's space: scaled, turned and moved by the parent.
        Some(parent) if depth < 16 => {
            let (po, ps, pa) = world_transform(parent, by_id, depth + 1);
            let (x, y) = (origin[0] * ps[0], origin[1] * ps[1]);
            let (c, sn) = (pa.cos(), pa.sin());
            ([po[0] + x * c - y * sn, po[1] + x * sn + y * c], [scale[0] * ps[0], scale[1] * ps[1]], angle + pa)
        }
        _ => (origin, scale, angle),
    }
}

/// Adds an object's effects to its layer: every pass of every effect, with the scene's own settings for them.
fn add_effects(assets: &Assets, obj: &Value, layer: &mut Layer, skipped: &mut Vec<String>) {
    for (n, effect) in obj["effects"].as_array().into_iter().flatten().enumerate() {
        if !truthy(effect.get("visible")) {
            continue;
        }
        let file = effect["file"].as_str().unwrap_or("");
        let Some(def) = assets.json(file) else {
            skipped.push(format!("missing effect {file}"));
            continue;
        };
        for fbo in def["fbos"].as_array().into_iter().flatten() {
            let scale = fbo["scale"].as_f64().unwrap_or(1.0).max(1.0) as f32;
            layer.fbos.push((fbo["name"].as_str().unwrap_or("").to_string(), scale));
        }
        let overrides = effect["passes"].as_array().cloned().unwrap_or_default();
        for (k, p) in def["passes"].as_array().into_iter().flatten().enumerate() {
            if p.get("command").is_some() {
                skipped.push(format!("effect command in {file}"));
                continue;
            }
            let Some(mut pass) = p["material"].as_str().and_then(|m| pass_from_material(assets, m, skipped)) else { continue };
            pass.target = p["target"].as_str().map(String::from);
            pass.effect = n + 1;
            pass.binds = p["bind"].as_array().into_iter().flatten().filter_map(|b| Some((b["index"].as_u64()? as usize, b["name"].as_str()?.to_string()))).collect();
            if let Some(o) = overrides.get(k) {
                if let Some(c) = o["combos"].as_object() {
                    pass.combos.extend(c.iter().map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0))));
                }
                if let Some(v) = o["constantshadervalues"].as_object() {
                    pass.values.extend(v.clone());
                }
                for (slot, t) in o["textures"].as_array().into_iter().flatten().enumerate() {
                    if let Some(t) = t.as_str() {
                        if pass.textures.len() <= slot {
                            pass.textures.resize(slot + 1, None);
                        }
                        pass.textures[slot] = Some(t.to_string());
                    }
                }
            }
            layer.passes.push(pass);
        }
    }
}

/// A text object: its font, its fixed text or the script that makes it (clocks, dates), and its box.
pub struct TextDef {
    /// Name of the generated texture its layer draws.
    texture: String,
    font: Option<Vec<u8>>,
    value: String,
    script: Option<String>,
    /// Script settings the scene changed from their defaults (like 12-hour clocks).
    props: Vec<(String, Value)>,
    padding: f32,
    align: String,
    box_size: [f32; 2],
    /// Its layer and scale, to resize the layer when the text grows past the box.
    layer: usize,
    scale: [f32; 2],
}

impl Scene {
    /// `settings`: the scene settings the user changed (name to value); the rest keep project.json's values.
    pub fn load(assets: &Assets, settings: &serde_json::Map<String, Value>) -> Result<Scene, String> {
        let mut json = assets.json("scene.json").ok_or("no scene.json")?;
        let mut values: serde_json::Map<String, Value> = assets.json("project.json").and_then(|p| {
            Some(p["general"]["properties"].as_object()?.iter().map(|(k, v)| (k.clone(), v["value"].clone())).collect())
        }).unwrap_or_default();
        values.extend(settings.clone());
        apply_settings(&mut json, &values);
        let general = &json["general"];
        let ortho = &general["orthogonalprojection"];
        let width = ortho["width"].as_f64().unwrap_or(1920.0) as f32;
        let height = ortho["height"].as_f64().unwrap_or(1080.0) as f32;
        let clear = floats(&general["clearcolor"]);
        let objects: Vec<&Value> = json["objects"].as_array().into_iter().flatten().collect();
        let by_id: HashMap<i64, &Value> = objects.iter().filter_map(|o| Some((o["id"].as_i64()?, *o))).collect();
        let mut skipped = Vec::new();
        let mut layers = Vec::new();
        let mut systems = Vec::new();
        let mut texts = Vec::new();
        let mut sounds = Vec::new();
        for (order, obj) in objects.iter().copied().enumerate() {
            let name = obj["name"].as_str().unwrap_or("?");
            if !truthy(obj.get("visible")) {
                continue;
            }
            let (center, scale, angle) = world_transform(obj, &by_id, 0);
            let alpha = floats(&obj["alpha"]).first().copied().unwrap_or(1.0);
            let color = floats(&obj["color"]).get(..3).map_or([1.0; 3], |c| [c[0], c[1], c[2]]);
            if let Some(text) = obj.get("text").filter(|t| t.is_object()) {
                // Text becomes a layer whose picture is drawn from the font, so its effects work like any layer's.
                let n = texts.len();
                let box_size = vec2_of(&obj["size"], [512.0, 128.0]);
                let brightness = floats(&obj["brightness"]).first().copied().unwrap_or(1.0);
                let font = obj["font"].as_str().and_then(|f| assets.get(f)).or_else(|| fs::read(r"C:\Windows\Fonts\segoeui.ttf").ok());
                texts.push(TextDef {
                    texture: format!("__text_{n}"),
                    font,
                    value: text["value"].as_str().unwrap_or("").to_string(),
                    script: text["script"].as_str().map(String::from),
                    props: text["scriptproperties"].as_object().map(|o| o.clone().into_iter().collect()).unwrap_or_default(),
                    padding: floats(&obj["padding"]).first().copied().unwrap_or(32.0),
                    align: obj["horizontalalign"].as_str().unwrap_or("center").to_string(),
                    box_size,
                    layer: layers.len(),
                    scale,
                });
                let base = Pass {
                    shader: "genericimage3".into(),
                    combos: HashMap::from([("VERSION".to_string(), 2)]),
                    textures: vec![Some(format!("__text_{n}"))],
                    values: HashMap::new(),
                    target: None,
                    binds: Vec::new(),
                    effect: 0,
                };
                let mut layer = Layer {
                    center,
                    size: [box_size[0] * scale[0], box_size[1] * scale[1]],
                    alpha,
                    color: color.map(|c| c * brightness),
                    passes: vec![base],
                    fbos: Vec::new(),
                    order,
                    angle,
                };
                add_effects(assets, obj, &mut layer, &mut skipped);
                layers.push(layer);
                continue;
            }
            let Some(image) = obj["image"].as_str() else {
                if let Some(file) = obj["particle"].as_str() {
                    for (k, def) in load_particles(assets, file, &mut skipped).into_iter().enumerate() {
                        systems.push(System::new(order, obj, def, 0x9E37_79B9 ^ ((order as u64) << 20) ^ k as u64, (center, scale, angle)));
                    }
                } else if let Some(files) = obj["sound"].as_array() {
                    // "Start silent" sounds wait for the scene's script (a click, a whoosh): they never play here.
                    if obj["startsilent"].as_bool() == Some(true) {
                        continue;
                    }
                    sounds.push(SoundDef {
                        files: files.iter().filter_map(|f| f.as_str().map(String::from)).collect(),
                        volume: floats(&obj["volume"]).first().copied().unwrap_or(1.0),
                        mode: obj["playbackmode"].as_str().unwrap_or("loop").to_string(),
                    });
                } else if obj.get("model").is_some() {
                    skipped.push(format!("3D model: {name}"));
                } else {
                    skipped.push(format!("object: {name}"));
                }
                continue;
            };
            let Some(model) = assets.json(image) else {
                skipped.push(format!("missing model {image}"));
                continue;
            };
            if model.get("puppet").is_some() {
                skipped.push(format!("puppet animation on {name} (shown still)"));
            }
            let Some(base) = model["material"].as_str().and_then(|m| pass_from_material(assets, m, &mut skipped)) else { continue };
            let mut size = floats(&obj["size"]);
            if size.len() < 2 {
                size = vec![width, height];
            }
            let mut layer = Layer {
                center: if obj.get("origin").is_some() { center } else { [width / 2.0, height / 2.0] },
                size: [size[0] * scale[0], size[1] * scale[1]],
                alpha,
                color,
                passes: vec![base],
                fbos: Vec::new(),
                order,
                angle,
            };
            add_effects(assets, obj, &mut layer, &mut skipped);
            layers.push(layer);
        }
        let clear = [clear.first().copied().unwrap_or(0.0), clear.get(1).copied().unwrap_or(0.0), clear.get(2).copied().unwrap_or(0.0)];
        Ok(Scene { width, height, clear, layers, systems, texts, sounds, skipped })
    }
}

// ---------- Renderer ----------

struct Target {
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    w: i32,
    h: i32,
}

impl Target {
    fn res(&self) -> [f32; 4] {
        [self.w as f32, self.h as f32, self.w as f32, self.h as f32]
    }
}

fn new_target(gl: &glow::Context, w: i32, h: i32) -> Target {
    let (w, h) = (w.max(1), h.max(1));
    unsafe {
        let tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
        for (p, v) in [(glow::TEXTURE_MIN_FILTER, glow::LINEAR), (glow::TEXTURE_MAG_FILTER, glow::LINEAR), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE)] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
        }
        let fbo = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
        Target { fbo, tex, w, h }
    }
}

/// Draws one texture into a rectangle (clip space x0 y0 x1 y1), tinted. Used to stack layers and show the frame.
/// Corners in clip space: u_C0 bottom left, u_C1 bottom right, u_C2 top left, u_C3 top right (of the picture),
/// so rotated layers work too.
const BLIT_VERT: &str = "#version 120
attribute vec3 a_Position; attribute vec2 a_TexCoord; varying vec2 v_UV; uniform vec2 u_C0, u_C1, u_C2, u_C3;
void main() { v_UV = a_TexCoord; vec2 t = a_Position.xy * 0.5 + 0.5; gl_Position = vec4(mix(mix(u_C0, u_C1, t.x), mix(u_C2, u_C3, t.x), t.y), 0.0, 1.0); }";

/// The four corners of an axis-aligned clip-space rectangle x0 y0 x1 y1, for draw_blit.
fn rect_corners(r: [f32; 4]) -> [[f32; 2]; 4] {
    [[r[0], r[1]], [r[2], r[1]], [r[0], r[3]], [r[2], r[3]]]
}
const BLIT_FRAG: &str = "#version 120
varying vec2 v_UV; uniform sampler2D u_Tex; uniform vec4 u_Tint;
void main() { gl_FragColor = texture2D(u_Tex, v_UV) * u_Tint; }";

/// The shader source of one Wallpaper Engine shader, with includes resolved, parsed once.
struct Source {
    vert: String,
    frag: String,
    uniforms: Vec<UniformInfo>,
    combos: HashMap<String, i64>,
    if_names: HashSet<String>,
}

/// Runs a scene with OpenGL. GL objects are made on the first frame, when the context exists.
///
/// Every buffer keeps image rows top first, like the decoded pictures, so Wallpaper Engine's shaders
/// (written for DirectX, where texture y points down) see textures the way they expect.
pub struct Renderer {
    assets: Assets,
    scene: Scene,
    started: Instant,
    /// Scene pixels per scene unit: big scenes are drawn smaller to save GPU.
    scale: f32,
    /// The widest the screen draws the scene, in pixels: pictures are kept up to this size.
    max_px: u32,
    quad: Option<glow::VertexArray>,
    blit: Option<glow::Program>,
    sources: HashMap<String, Option<Source>>,
    programs: HashMap<String, Option<glow::Program>>,
    textures: HashMap<String, Option<(glow::Texture, [f32; 4])>>,
    /// Per texture: sprite sheet frames and channel count.
    tex_extra: HashMap<String, (Vec<[f32; 4]>, u8)>,
    /// Per layer: two buffers that effect passes take turns writing, plus the effects' named buffers.
    targets: Vec<(Target, Target, HashMap<String, Target>)>,
    frame: Option<Target>,
    particles: Option<(glow::Program, glow::VertexArray, glow::Buffer)>,
    verts: Vec<f32>,
    last: f32,
    texts: Vec<TextRun>,
    /// Set once something was drawn that changes over time (particles, a shader reading g_Time).
    /// A scene without any is still: one frame stays right until a text changes.
    pub moving: bool,
    /// (layer, effect) pairs with a step that can't run: skipped whole.
    broken: HashSet<(usize, usize)>,
    pub errors: Vec<String>,
}

impl Renderer {
    pub fn new(assets: Assets, mut scene: Scene, max_width: f32) -> Renderer {
        let scale = (max_width / scene.width).min(1.0);
        let max_px = max_width as u32;
        let texts = std::mem::take(&mut scene.texts).into_iter().map(TextRun::new).collect();
        Renderer {
            max_px,
            texts,
            assets,
            scene,
            started: Instant::now(),
            scale,
            quad: None,
            blit: None,
            sources: HashMap::new(),
            programs: HashMap::new(),
            textures: HashMap::new(),
            tex_extra: HashMap::new(),
            targets: Vec::new(),
            frame: None,
            particles: None,
            verts: Vec::new(),
            last: 0.0,
            moving: false,
            broken: HashSet::new(),
            errors: Vec::new(),
        }
    }

    pub fn skipped(&self) -> &[String] {
        &self.scene.skipped
    }

    pub fn aspect(&self) -> f32 {
        self.scene.width / self.scene.height
    }

    fn error(&mut self, e: String) {
        if !self.errors.contains(&e) {
            self.errors.push(e);
        }
    }

    fn texture(&mut self, gl: &glow::Context, name: &str) -> Option<(glow::Texture, [f32; 4])> {
        if !self.textures.contains_key(name) {
            let file = format!("materials/{name}.tex");
            let loaded = match self.assets.get(&file).ok_or_else(|| format!("missing {file}")).and_then(|b| decode_tex(&b, self.max_px)) {
                Ok(t) => unsafe {
                    let tex = gl.create_texture().unwrap();
                    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, t.w as i32, t.h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&t.rgba)));
                    gl.generate_mipmap(glow::TEXTURE_2D);
                    for (p, v) in [(glow::TEXTURE_MIN_FILTER, glow::LINEAR_MIPMAP_LINEAR), (glow::TEXTURE_MAG_FILTER, glow::LINEAR), (glow::TEXTURE_WRAP_S, glow::REPEAT), (glow::TEXTURE_WRAP_T, glow::REPEAT)] {
                        gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
                    }
                    self.tex_extra.insert(name.to_string(), (t.frames.clone(), t.channels));
                    Some((tex, [t.w as f32, t.h as f32, t.img_w as f32, t.img_h as f32]))
                },
                Err(e) => {
                    self.error(format!("{name}: {e}"));
                    None
                }
            };
            self.textures.insert(name.to_string(), loaded);
        }
        self.textures[name]
    }

    /// A 1×1 picture of one color (kept, like loaded textures), for masks the scene never painted.
    fn solid(&mut self, gl: &glow::Context, rgba: [u8; 4]) -> (glow::Texture, [f32; 4]) {
        let name = format!("__solid_{rgba:?}");
        if let Some(Some(t)) = self.textures.get(&name) {
            return *t;
        }
        unsafe {
            let tex = gl.create_texture().unwrap();
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, 1, 1, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&rgba)));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            self.textures.insert(name.clone(), Some((tex, [1.0; 4])));
            self.tex_extra.insert(name, (Vec::new(), 4));
            (tex, [1.0; 4])
        }
    }

    fn source(&mut self, shader: &str) -> bool {
        if !self.sources.contains_key(shader) {
            let read = |ext: &str| {
                let text = self.assets.text(&format!("shaders/{shader}.{ext}"))?;
                Some(resolve_includes(&hoist_samplers(&text), &self.assets, &mut HashSet::new()))
            };
            let src = match (read("vert").map(|v| flip_unprojected(&v)), read("frag").map(|f| first_channel(&f))) {
                (Some(vert), Some(frag)) => {
                    let all = format!("{vert}\n{frag}");
                    Some(Source { uniforms: uniforms(&all), combos: combo_defaults(&all), if_names: if_names(&all), vert, frag })
                }
                _ => {
                    self.error(format!("missing shader {shader}"));
                    None
                }
            };
            self.sources.insert(shader.to_string(), src);
        }
        self.sources[shader].is_some()
    }

    /// The compiled program for a pass, with its combo switches set. Compiled once per shader and switches.
    fn program(&mut self, gl: &glow::Context, pass: &Pass, bound: &[bool]) -> Option<glow::Program> {
        if !self.source(&pass.shader) {
            return None;
        }
        let src = self.sources[&pass.shader].as_ref().unwrap();
        let mut combos = src.combos.clone();
        // A texture slot whose uniform names a combo (like MASK) switches it on when a texture is bound.
        for u in &src.uniforms {
            if let (Some(c), Some(slot)) = (u.note["combo"].as_str(), u.name.strip_prefix("g_Texture").and_then(|s| s.parse::<usize>().ok())) {
                combos.insert(c.to_string(), bound.get(slot).copied().unwrap_or(false) as i64);
            }
        }
        combos.extend(pass.combos.iter().map(|(k, v)| (k.clone(), *v)));
        let own = own_defines(&format!("{}\n{}", src.vert, src.frag));
        combos.retain(|k, _| !own.contains(k));
        let mut set: Vec<_> = combos.into_iter().collect();
        set.sort();
        let key = format!("{}|{set:?}", pass.shader);
        if !self.programs.contains_key(&key) {
            let mut defines: String = set.iter().map(|(k, v)| format!("#define {k} {v}\n")).collect();
            for n in &src.if_names {
                if !set.iter().any(|(k, _)| k == n) && !own.contains(n) {
                    defines += &format!("#define {n} 0\n");
                }
            }
            let mut helpers = helpers();
            if src.vert.contains("inverse(") || src.frag.contains("inverse(") {
                helpers += INVERSE;
            }
            let build = |body: &str| format!("{PREAMBLE}{defines}{helpers}{}", rename_calls(body));
            let result = compile(gl, &build(&src.vert), &build(&src.frag));
            let program = match result {
                Ok(p) => Some(p),
                Err(e) => {
                    let first: Vec<&str> = e.lines().filter(|l| !l.trim().is_empty()).take(3).collect();
                    self.error(format!("{}: {}", pass.shader, first.join(" | ")));
                    None
                }
            };
            self.programs.insert(key.clone(), program);
        }
        self.programs[&key]
    }

    /// Draws the whole scene, then shows it in the given viewport of the current window.
    /// `output` is where the finished frame goes (None: the window); `top_first` is for DirectX textures, whose
    /// rows run top first, unlike an OpenGL window's.
    pub fn paint(&mut self, gl: &glow::Context, viewport: [i32; 4], output: Option<glow::Framebuffer>, top_first: bool) {
        unsafe {
            if self.quad.is_none() {
                // A quad over the whole target; texture y 0 at the top edge.
                let verts: [f32; 20] = [-1.0, -1.0, 0.0, 0.0, 1.0, 1.0, -1.0, 0.0, 1.0, 1.0, -1.0, 1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0];
                let vao = gl.create_vertex_array().unwrap();
                gl.bind_vertex_array(Some(vao));
                let vbo = gl.create_buffer().unwrap();
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                let bytes: Vec<u8> = verts.iter().flat_map(|f| f.to_le_bytes()).collect();
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
                gl.enable_vertex_attrib_array(0);
                gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, 20, 0);
                gl.enable_vertex_attrib_array(1);
                gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 20, 12);
                self.quad = Some(vao);
                match compile(gl, BLIT_VERT, BLIT_FRAG) {
                    Ok(p) => self.blit = Some(p),
                    Err(e) => self.error(format!("blit: {e}")),
                }
            }
            let Some(blit) = self.blit else { return };
            gl.bind_vertex_array(self.quad);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            let time = self.started.elapsed().as_secs_f32();
            let (fw, fh) = ((self.scene.width * self.scale) as i32, (self.scene.height * self.scale) as i32);
            if self.frame.is_none() {
                self.frame = Some(new_target(gl, fw, fh));
            }
            let frame_fbo = self.frame.as_ref().unwrap().fbo;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(frame_fbo));
            gl.viewport(0, 0, fw, fh);
            let c = self.scene.clear;
            gl.clear_color(c[0], c[1], c[2], 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            self.update_texts(gl);

            // Image layers and particle systems, drawn in the scene's object order.
            let mut items: Vec<(usize, bool, usize)> = self.scene.layers.iter().enumerate().map(|(i, l)| (l.order, true, i)).collect();
            items.extend(self.scene.systems.iter().enumerate().map(|(i, s)| (s.order, false, i)));
            items.sort();
            let dt = (time - self.last).clamp(0.0, 0.1);
            self.last = time;
            for (_, is_layer, li) in items {
                if !is_layer {
                    self.draw_particles(gl, li, dt, time, frame_fbo, fw, fh);
                    gl.bind_vertex_array(self.quad);
                    continue;
                }
                let (lw, lh) = ((self.scene.layers[li].size[0] * self.scale) as i32, (self.scene.layers[li].size[1] * self.scale) as i32);
                // A text layer that changed width (new clock digits, new day) gets new targets.
                if self.targets.get(li).is_some_and(|t| (t.0.w, t.0.h) != (lw.max(1), lh.max(1))) {
                    let (a, b, named) = self.targets.remove(li);
                    for t in [a, b].into_iter().chain(named.into_values()) {
                        gl.delete_framebuffer(t.fbo);
                        gl.delete_texture(t.tex);
                    }
                    let mut named = HashMap::new();
                    for (name, div) in &self.scene.layers[li].fbos {
                        named.insert(name.clone(), new_target(gl, (lw as f32 / div) as i32, (lh as f32 / div) as i32));
                    }
                    self.targets.insert(li, (new_target(gl, lw, lh), new_target(gl, lw, lh), named));
                }
                if self.targets.len() <= li {
                    let mut named = HashMap::new();
                    for (name, div) in &self.scene.layers[li].fbos {
                        named.insert(name.clone(), new_target(gl, (lw as f32 / div) as i32, (lh as f32 / div) as i32));
                    }
                    self.targets.push((new_target(gl, lw, lh), new_target(gl, lw, lh), named));
                }
                let mut current_is_a = true;
                for pi in 0..self.scene.layers[li].passes.len() {
                    // An effect with a step that can't run is skipped whole: its later steps would read an empty
                    // buffer and paint the layer black (strict drivers, AMD's, refuse some Workshop shaders).
                    let effect = self.scene.layers[li].passes[pi].effect;
                    if self.broken.contains(&(li, effect)) {
                        continue;
                    }
                    if !self.run_pass(gl, li, pi, &mut current_is_a, time) && effect > 0 {
                        self.broken.insert((li, effect));
                    }
                }
                // Stack the layer onto the frame. The frame keeps rows top first too, so scene y is flipped.
                let layer = &self.scene.layers[li];
                let (w, h) = (self.scene.width, self.scene.height);
                // Corners around the layer's center, turned by its angle (counterclockwise, y up), then to clip space.
                let (ca, sa) = (layer.angle.cos(), layer.angle.sin());
                let (hw, hh) = (layer.size[0] / 2.0, layer.size[1] / 2.0);
                let corner = |dx: f32, dy: f32| {
                    let (x, y) = (layer.center[0] + dx * ca - dy * sa, layer.center[1] + dx * sa + dy * ca);
                    [2.0 * x / w - 1.0, 1.0 - 2.0 * y / h]
                };
                let rect = [corner(-hw, -hh), corner(hw, -hh), corner(-hw, hh), corner(hw, hh)];
                let tint = [layer.color[0], layer.color[1], layer.color[2], layer.alpha];
                let (a, bb, _) = &self.targets[li];
                let tex = if current_is_a { a.tex } else { bb.tex };
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(frame_fbo));
                gl.viewport(0, 0, fw, fh);
                gl.enable(glow::BLEND);
                gl.blend_func_separate(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA, glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                self.draw_blit(gl, blit, tex, rect, tint);
                gl.disable(glow::BLEND);
            }

            // Show the frame.
            gl.bind_framebuffer(glow::FRAMEBUFFER, output);
            gl.viewport(viewport[0], viewport[1], viewport[2], viewport[3]);
            let frame_tex = self.frame.as_ref().unwrap().tex;
            let rect = if top_first { [-1.0, 1.0, 1.0, -1.0] } else { [-1.0, -1.0, 1.0, 1.0] };
            self.draw_blit(gl, blit, frame_tex, rect_corners(rect), [1.0; 4]);
            gl.bind_vertex_array(None);
            gl.use_program(None);
        }
    }

    /// Once a second, asks each text object for its text (clocks run their script). True when one changed and
    /// needs drawing. No OpenGL here, so a still scene can ask without drawing a frame.
    pub fn refresh_texts(&mut self) -> bool {
        let now = Instant::now();
        for i in 0..self.texts.len() {
            if now < self.texts[i].next {
                continue;
            }
            self.texts[i].next = now + Duration::from_secs(1);
            let value = self.texts[i].value();
            if let Some(e) = self.texts[i].error.take() {
                self.error(e);
            }
            if self.texts[i].shown.as_deref() != Some(value.as_str()) {
                self.texts[i].want = Some(value);
            }
        }
        self.texts.iter().any(|t| t.want.is_some())
    }

    /// Draws the texts whose text changed into their textures.
    unsafe fn update_texts(&mut self, gl: &glow::Context) {
        self.refresh_texts();
        for i in 0..self.texts.len() {
            let Some(value) = self.texts[i].want.take() else { continue };
            let def = &self.texts[i].def;
            let rows = def.value.lines().count().max(1);
            let drawn = def.font.as_deref().and_then(|f| render_text(f, &value, def.box_size, def.padding, &def.align, rows));
            let name = def.texture.clone();
            let Some((w, h, rgba, resolution)) = drawn else {
                self.error(format!("can't draw text {value:?}"));
                self.texts[i].shown = Some(value);
                continue;
            };
            if let Some(Some((old, _))) = self.textures.remove(&name) {
                gl.delete_texture(old);
            }
            let tex = gl.create_texture().unwrap();
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w as i32, h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(&rgba)));
            for (p, v) in [(glow::TEXTURE_MIN_FILTER, glow::LINEAR), (glow::TEXTURE_MAG_FILTER, glow::LINEAR), (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE), (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE)] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
            }
            let (layer, scale) = (def.layer, def.scale);
            // A text drawn smaller to fit 4096 px keeps its real size on screen.
            self.scene.layers[layer].size = [w as f32 / resolution * scale[0], h as f32 / resolution * scale[1]];
            self.textures.insert(name.clone(), Some((tex, [w as f32, h as f32, w as f32, h as f32])));
            self.tex_extra.insert(name, (Vec::new(), 4));
            self.texts[i].shown = Some(value);
        }
    }

    /// Moves a particle system on by `dt` seconds and draws it onto the frame.
    unsafe fn draw_particles(&mut self, gl: &glow::Context, si: usize, dt: f32, time: f32, frame_fbo: glow::Framebuffer, fw: i32, fh: i32) {
        self.moving = true;
        if self.particles.is_none() {
            let program = match compile(gl, PARTICLE_VERT, PARTICLE_FRAG) {
                Ok(p) => p,
                Err(e) => return self.error(format!("particles: {e}")),
            };
            let vao = gl.create_vertex_array().unwrap();
            gl.bind_vertex_array(Some(vao));
            let vbo = gl.create_buffer().unwrap();
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            for (name, size, offset) in [("a_Pos", 2, 0), ("a_UV", 2, 8), ("a_Color", 4, 16)] {
                if let Some(loc) = gl.get_attrib_location(program, name) {
                    gl.enable_vertex_attrib_array(loc);
                    gl.vertex_attrib_pointer_f32(loc, size, glow::FLOAT, false, 32, offset);
                }
            }
            self.particles = Some((program, vao, vbo));
        }
        let (program, vao, vbo) = self.particles.unwrap();
        let name = self.scene.systems[si].def.texture.clone();
        let tex = name.as_deref().and_then(|n| self.texture(gl, n));
        let (frames, channels) = name.and_then(|n| self.tex_extra.get(&n).cloned()).unwrap_or((Vec::new(), 4));
        let sys = &mut self.scene.systems[si];
        // "starttime" means the system starts as if it had already run that long.
        let warm = std::mem::take(&mut sys.def.prewarm);
        for k in 0..(warm * 30.0) as usize {
            sys.step(1.0 / 30.0, k as f32 / 30.0, frames.len());
        }
        sys.step(dt, time, frames.len());
        self.verts.clear();
        sys.vertices(&mut self.verts, &frames, self.scene.width, self.scene.height);
        if self.verts.is_empty() {
            return;
        }
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let bytes = std::slice::from_raw_parts(self.verts.as_ptr() as *const u8, self.verts.len() * 4);
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(frame_fbo));
        gl.viewport(0, 0, fw, fh);
        gl.enable(glow::BLEND);
        if self.scene.systems[si].def.additive {
            gl.blend_func_separate(glow::SRC_ALPHA, glow::ONE, glow::ZERO, glow::ONE);
        } else {
            gl.blend_func_separate(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA, glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        }
        gl.use_program(Some(program));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, tex.map(|t| t.0));
        gl.uniform_1_i32(gl.get_uniform_location(program, "u_Tex").as_ref(), 0);
        let mode = if tex.is_none() { 3 } else { match channels { 1 => 1, 2 => 2, _ => 0 } };
        gl.uniform_1_i32(gl.get_uniform_location(program, "u_Mode").as_ref(), mode);
        gl.draw_arrays(glow::TRIANGLES, 0, (self.verts.len() / 8) as i32);
        gl.disable(glow::BLEND);
    }

    unsafe fn draw_blit(&self, gl: &glow::Context, blit: glow::Program, tex: glow::Texture, corners: [[f32; 2]; 4], tint: [f32; 4]) {
        gl.use_program(Some(blit));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.uniform_1_i32(gl.get_uniform_location(blit, "u_Tex").as_ref(), 0);
        for (k, c) in corners.iter().enumerate() {
            gl.uniform_2_f32(gl.get_uniform_location(blit, &format!("u_C{k}")).as_ref(), c[0], c[1]);
        }
        gl.uniform_4_f32(gl.get_uniform_location(blit, "u_Tint").as_ref(), tint[0], tint[1], tint[2], tint[3]);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
    }

    /// Runs one pass of a layer: pass 0 draws the layer's image, later passes take the previous result as texture 0.
    /// False when the pass can't run (its shader failed to build, or it names an unknown buffer).
    unsafe fn run_pass(&mut self, gl: &glow::Context, li: usize, pi: usize, current_is_a: &mut bool, time: f32) -> bool {
        let pass = &self.scene.layers[li].passes[pi];
        let (pass_textures, pass_binds, pass_target) = (pass.textures.clone(), pass.binds.clone(), pass.target.clone());
        let shader = pass.shader.clone();
        let current = {
            let (a, b, _) = &self.targets[li];
            let t = if *current_is_a { a } else { b };
            (t.tex, t.res())
        };
        // Texture slots: what the pass names, with "_rt_" names meaning this layer's buffers.
        let mut slots: Vec<Option<(glow::Texture, [f32; 4])>> = Vec::new();
        for (slot, name) in pass_textures.iter().enumerate() {
            let t = match name {
                _ if slot == 0 && pi > 0 => Some(current),
                Some(n) if n.starts_with("_rt_") => self.buffer(li, n, current),
                Some(n) => self.texture(gl, n),
                None => None,
            };
            slots.push(t);
        }
        if pi > 0 && slots.is_empty() {
            slots.push(Some(current));
        }
        for (index, name) in &pass_binds {
            if slots.len() <= *index {
                slots.resize(*index + 1, None);
            }
            slots[*index] = if name == "previous" { Some(current) } else { self.buffer(li, name, current) };
        }
        // Slots the shader declares with a default texture (like util/white) get it when the pass names none.
        if self.source(&shader) {
            let defaults: Vec<(usize, String)> = self.sources[&shader]
                .as_ref()
                .unwrap()
                .uniforms
                .iter()
                .filter_map(|u| Some((u.name.strip_prefix("g_Texture")?.parse::<usize>().ok()?, u.note["default"].as_str()?.to_string())))
                .collect();
            for (slot, name) in defaults {
                // "_alias_" names are editor placeholders (like a light cookie), not real textures.
                if name.starts_with("_alias_") {
                    continue;
                }
                if slots.get(slot).is_none_or(|s| s.is_none()) {
                    let t = if name.starts_with("_rt_") { self.buffer(li, &name, current) } else { self.texture(gl, &name) };
                    if slots.len() <= slot {
                        slots.resize(slot + 1, None);
                    }
                    slots[slot] = t;
                }
            }
            // Masks the scene switched on (their "require" combos are set, like ENABLEMASK) but never painted have a
            // paint color instead: an unpainted mask is that color everywhere, and black means the effect shows nowhere.
            // Left empty, the shader would read whatever picture was bound before and apply the effect everywhere.
            // Masks without a switch stay empty: their MASK combo is then off and the effect covers the whole layer.
            let combos = self.scene.layers[li].passes[pi].combos.clone();
            let painted: Vec<(usize, [u8; 4])> = self.sources[&shader]
                .as_ref()
                .unwrap()
                .uniforms
                .iter()
                .filter_map(|u| {
                    let slot = u.name.strip_prefix("g_Texture")?.parse::<usize>().ok()?;
                    let color = floats(u.note.get("paintdefaultcolor")?);
                    let require = u.note["require"].as_object().filter(|r| !r.is_empty())?;
                    let switched_on = require.iter().all(|(k, v)| combos.get(k).copied() == v.as_i64());
                    switched_on.then(|| (slot, [0, 1, 2, 3].map(|k| (color.get(k).copied().unwrap_or(1.0).clamp(0.0, 1.0) * 255.0) as u8)))
                })
                .collect();
            for (slot, rgba) in painted {
                if slots.get(slot).is_none_or(|s| s.is_none()) {
                    if slots.len() <= slot {
                        slots.resize(slot + 1, None);
                    }
                    slots[slot] = Some(self.solid(gl, rgba));
                }
            }
        }
        let bound: Vec<bool> = slots.iter().map(Option::is_some).collect();
        let pass = &self.scene.layers[li].passes[pi];
        let pass_values = pass.values.clone();
        let pass_clone = Pass { shader: shader.clone(), combos: pass.combos.clone(), textures: Vec::new(), values: HashMap::new(), target: None, binds: Vec::new(), effect: 0 };
        let Some(program) = self.program(gl, &pass_clone, &bound) else { return false };

        // Where it draws: a named buffer, or the layer buffer not holding the current result.
        let (fbo, tw, th) = {
            let (a, b, named) = &self.targets[li];
            let out = match &pass_target {
                Some(n) => named.get(n),
                None if pi == 0 => Some(a),
                None => Some(if *current_is_a { b } else { a }),
            };
            match out {
                Some(t) => (t.fbo, t.w, t.h),
                None => {
                    self.error(format!("unknown buffer {pass_target:?}"));
                    return false;
                }
            }
        };
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.viewport(0, 0, tw, th);
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
        gl.use_program(Some(program));

        let uniforms: Vec<(String, String, Value)> = self.sources[&shader].as_ref().unwrap().uniforms.iter().map(|u| (u.ty.clone(), u.name.clone(), u.note.clone())).collect();
        let (sw, sh) = (self.scene.width, self.scene.height);
        for (ty, name, note) in uniforms {
            let Some(loc) = gl.get_uniform_location(program, &name) else { continue };
            let loc = Some(&loc);
            let v: Vec<f32> = match name.as_str() {
                // Draw the pass flipped, so buffers keep rows top first.
                "g_ModelViewProjectionMatrix" => {
                    gl.uniform_matrix_4_f32_slice(loc, false, &[1.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
                    continue;
                }
                "g_EffectTextureProjectionMatrix" | "g_EffectTextureProjectionMatrixInverse" => {
                    gl.uniform_matrix_4_f32_slice(loc, false, &[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]);
                    continue;
                }
                "g_Time" => {
                    self.moving = true;
                    vec![time]
                }
                "g_Daytime" => vec![(time / 86_400.0).fract()],
                "g_Screen" => vec![sw, sh, sw / sh],
                "g_PointerPosition" | "g_PointerPositionLast" => vec![0.5, 0.5],
                "g_TexelSize" => vec![1.0 / current.1[0], 1.0 / current.1[1]],
                "g_TexelSizeHalf" => vec![0.5 / current.1[0], 0.5 / current.1[1]],
                "g_Texture0Rotation" => vec![1.0, 0.0, 0.0, 1.0],
                "g_Texture0Translation" => vec![0.0, 0.0],
                "g_Color4" => vec![1.0, 1.0, 1.0, 1.0],
                "g_Color" => vec![1.0, 1.0, 1.0],
                "g_Alpha" | "g_Brightness" | "g_UserAlpha" => vec![1.0],
                _ if ty.starts_with("sampler") => {
                    let slot: i32 = name.strip_prefix("g_Texture").and_then(|s| s.parse().ok()).unwrap_or(0);
                    gl.uniform_1_i32(loc, slot);
                    continue;
                }
                n if n.starts_with("g_Texture") && n.ends_with("Resolution") => {
                    let slot: usize = n.trim_start_matches("g_Texture").trim_end_matches("Resolution").parse().unwrap_or(0);
                    slots.get(slot).copied().flatten().map_or(current.1, |s| s.1).to_vec()
                }
                _ => {
                    let key = note["material"].as_str().unwrap_or("");
                    let v = pass_values.get(key).map(floats).filter(|v| !v.is_empty()).unwrap_or_else(|| floats(&note["default"]));
                    if v.is_empty() {
                        continue;
                    }
                    v
                }
            };
            let g = |i: usize| v.get(i).copied().unwrap_or(0.0);
            match ty.as_str() {
                "float" => gl.uniform_1_f32(loc, g(0)),
                "vec2" => gl.uniform_2_f32(loc, g(0), g(1)),
                "vec3" => gl.uniform_3_f32(loc, g(0), g(1), g(2)),
                "vec4" => gl.uniform_4_f32(loc, g(0), g(1), g(2), g(3)),
                "int" | "bool" => gl.uniform_1_i32(loc, g(0) as i32),
                _ => {}
            }
        }
        for (slot, t) in slots.iter().enumerate() {
            if let Some((tex, _)) = t {
                gl.active_texture(glow::TEXTURE0 + slot as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(*tex));
            }
        }
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.active_texture(glow::TEXTURE0);
        if pass_target.is_none() && pi > 0 {
            *current_is_a = !*current_is_a;
        }
        true
    }

    /// A named render buffer: the layer's own two ("_rt_imageLayerComposite_<id>_a/_b"), the frame so far, or an effect's.
    fn buffer(&self, li: usize, name: &str, current: (glow::Texture, [f32; 4])) -> Option<(glow::Texture, [f32; 4])> {
        let (a, b, named) = &self.targets[li];
        if name.starts_with("_rt_imageLayerComposite_") {
            let t = if name.ends_with("_b") { b } else { a };
            return Some((t.tex, t.res()));
        }
        if name == "_rt_FullFrameBuffer" || name == "_rt_MipMappedFrameBuffer" {
            return self.frame.as_ref().map(|f| (f.tex, f.res()));
        }
        named.get(name).map(|t| (t.tex, t.res())).or(Some(current))
    }
}

// ---------- Preview window ----------

struct Preview {
    renderer: Arc<Mutex<Renderer>>,
    last: Instant,
    fps: f32,
}

impl eframe::App for Preview {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dt = self.last.elapsed().as_secs_f32();
        self.last = Instant::now();
        self.fps = self.fps * 0.95 + (1.0 / dt.max(0.001)) * 0.05;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK)).show(ctx, |ui| {
            let avail = ui.available_rect_before_wrap();
            let aspect = self.renderer.lock().unwrap().aspect();
            let size = if avail.width() / avail.height() > aspect { egui::vec2(avail.height() * aspect, avail.height()) } else { egui::vec2(avail.width(), avail.width() / aspect) };
            let rect = egui::Rect::from_center_size(avail.center(), size);
            let r = self.renderer.clone();
            let callback = egui_glow::CallbackFn::new(move |info, painter| {
                let vp = info.viewport_in_pixels();
                r.lock().unwrap().paint(painter.gl(), [vp.left_px, vp.from_bottom_px, vp.width_px, vp.height_px], None, false);
            });
            ui.painter().add(egui::PaintCallback { rect, callback: Arc::new(callback) });
            let r = self.renderer.lock().unwrap();
            let mut text = format!("{:.0} fps", self.fps);
            for s in r.skipped() {
                text += &format!("\nskipped: {s}");
            }
            for e in &r.errors {
                text += &format!("\nerror: {e}");
            }
            ui.painter().text(avail.left_top() + egui::vec2(8.0, 8.0), egui::Align2::LEFT_TOP, text, egui::FontId::monospace(12.0), egui::Color32::from_rgb(255, 230, 120));
        });
        ctx.request_repaint();
    }
}

/// Opens a scene folder (a Workshop item with scene.pkg) in a preview window. For `wisp.exe --scene <folder>`.
pub fn preview(folder: &str) {
    let folder = Path::new(folder);
    let assets = match Assets::open(folder) {
        Some(a) => a,
        None => {
            message(&format!("No scene.pkg in {}", folder.display()), MB_ICONWARNING);
            return;
        }
    };
    let scene = match Scene::load(&assets, &serde_json::Map::new()) {
        Ok(s) => s,
        Err(e) => {
            message(&format!("Can't read this scene: {e}"), MB_ICONWARNING);
            return;
        }
    };
    let renderer = Arc::new(Mutex::new(Renderer::new(assets, scene, 1920.0)));
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_title("Wisp scene preview").with_inner_size([1280.0, 760.0]), ..Default::default() };
    let _ = eframe::run_native("Wisp scene preview", options, Box::new(move |_| Ok(Box::new(Preview { renderer, last: Instant::now(), fps: 60.0 }))));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_clock_script() {
        // Trimmed from a real Workshop clock: settings builder, export syntax, Date, and a scene override.
        let script = "'use strict';\nexport var scriptProperties = createScriptProperties()\n.addCheckbox({ name: 'use24hFormat', value: true })\n.addText({ name: 'delimiter', value: ':' })\n.finish();\nexport function update(value) {\n let t = new Date(2026, 10, 15, 21, 5);\n var h = t.getHours();\n if (!scriptProperties.use24hFormat) { h %= 12; if (h == 0) h = 12; }\n return ('00' + h).slice(-2) + scriptProperties.delimiter + ('00' + t.getMinutes()).slice(-2);\n}";
        let def = |props: Vec<(String, Value)>| TextDef { texture: String::new(), font: None, value: "x".into(), script: Some(script.into()), props, padding: 0.0, align: "center".into(), box_size: [10.0, 10.0], layer: 0, scale: [1.0, 1.0] };
        assert_eq!(TextRun::new(def(Vec::new())).value(), "21:05");
        let twelve = vec![("use24hFormat".to_string(), serde_json::json!({"value": false})), ("delimiter".to_string(), serde_json::json!("."))];
        assert_eq!(TextRun::new(def(twelve)).value(), "09.05");
        // A script that throws keeps the old text instead of breaking the scene.
        let mut bad = TextRun::new(TextDef { script: Some("export function update(v) { throw new Error('boom'); }".into()), ..def(Vec::new()) });
        assert_eq!(bad.value(), "x");
        assert!(bad.error.as_deref().is_some_and(|e| e.contains("boom")));
        // `export const` scripts are found too.
        let arrow = TextDef { script: Some("export const update = (v) => 'ok';".into()), ..def(Vec::new()) };
        assert_eq!(TextRun::new(arrow).value(), "ok");
        let mut idle = TextRun::new(TextDef { script: Some("export function init(v) { return v; }".into()), ..def(Vec::new()) });
        assert!(idle.value() == "x" && idle.error.is_none());
        // Endless loops are stopped, at load and in update(), and the placeholder stays.
        let start = Instant::now();
        let stuck = TextRun::new(TextDef { script: Some("while (engine.runtime < 1) {}".into()), ..def(Vec::new()) });
        assert!(stuck.js.is_none() && stuck.error.is_some());
        let mut spin = TextRun::new(TextDef { script: Some("export function update(v) { for (;;) {} }".into()), ..def(Vec::new()) });
        assert_eq!(spin.value(), "x");
        assert_eq!(spin.value(), "x");
        assert!(start.elapsed() < Duration::from_secs(2));
        // A script that fails to load says why.
        let broken = TextRun::new(TextDef { script: Some("export function update( {".into()), ..def(Vec::new()) });
        assert!(broken.error.as_deref().is_some_and(|e| e.contains("SyntaxError")));
    }

    /// A scene's song plays from inside scene.pkg and stops with the setting. Plays very quietly on this PC's
    /// speakers; needs scene 3411756828 (it has a song) and libmpv: run with --ignored.
    #[test]
    #[ignore]
    fn plays_scene_music() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        let assets = Assets::open(Path::new(r"C:\Program Files (x86)\Steam\steamapps\workshop\content\431960\3411756828")).unwrap();
        let mut scene = Scene::load(&assets, &serde_json::Map::new()).unwrap();
        assert_eq!(scene.sounds.len(), 1);
        assert!(assets.media_url(&scene.sounds[0].files[0]).unwrap().starts_with("slice://"));
        let mut sound = SceneAudio::new(std::mem::take(&mut scene.sounds));
        let audio = crate::mpv::Audio { on: AtomicBool::new(true), volume: AtomicU32::new(1) };
        sound.follow(&assets, Some(&audio));
        assert_eq!(sound.players.len(), 1);
        let until = Instant::now() + Duration::from_secs(5);
        while sound.players[0].0.get("current-tracks/audio/codec").is_none() {
            assert!(Instant::now() < until, "the song never started");
            std::thread::sleep(Duration::from_millis(50));
        }
        audio.on.store(false, Ordering::Relaxed);
        sound.follow(&assets, Some(&audio));
        assert!(sound.players.is_empty());
    }

    #[test]
    fn applies_scene_settings() {
        let mut scene = serde_json::json!({"objects": [
            {"visible": {"user": "clock", "value": true}, "alpha": {"user": "fade", "value": 0.5}},
            {"visible": {"user": {"name": "style", "condition": "2"}, "value": false}},
            {"sound": ["a.mp3"], "volume": {"user": "music", "value": 1}},
        ]});
        let mut used = HashSet::new();
        used_settings(&scene, &mut used);
        assert_eq!(used, HashSet::from(["clock".to_string(), "fade".to_string(), "style".to_string(), "music".to_string()]));
        let values = serde_json::json!({"clock": false, "style": 2}).as_object().unwrap().clone();
        apply_settings(&mut scene, &values);
        let o = &scene["objects"];
        // Set: the chosen value. Not set: the binding's default. Combo condition: true when it matches.
        assert_eq!((&o[0]["visible"], &o[0]["alpha"], &o[1]["visible"]), (&Value::Bool(false), &serde_json::json!(0.5), &Value::Bool(true)));
    }

    #[test]
    fn keeps_first_channel() {
        let f = "\tfloat mask = texSample2D(g_Texture1, v_TexCoord.xy);\n\tfloat a = texSample2D(t, uv).r * 2.0;\n\tvec4 c = texSample2D(t, uv);\n\tfloat b = texSample2D(t, f(uv)) + texSample2D(t, uv);";
        let out = first_channel(f);
        assert!(out.contains("float mask = texSample2D(g_Texture1, v_TexCoord.xy).x;"));
        // Already a single channel, not a float, or more than one sample: unchanged.
        assert_eq!(out.matches(".x;").count(), 1);
    }

    #[test]
    fn flips_unprojected_passes() {
        let v = "void main() {\n\tgl_Position = vec4(a_Position, 1.0);\n\tgl_Position = mul(vec4(a_Position, 1.0), g_ModelViewProjectionMatrix);\n\tv_ScreenPos = gl_Position.xyw;\n\tgl_Position = vec4(position, 1.0);\n}";
        let out = flip_unprojected(v);
        assert_eq!(out.matches("gl_Position.y = -gl_Position.y;").count(), 1);
        assert!(out.contains("vec4(a_Position, 1.0); gl_Position.y = -gl_Position.y;"));
    }

    #[test]
    fn draws_text() {
        let font = fs::read(r"C:\Windows\Fonts\segoeui.ttf").unwrap();
        let (w, h, px, _) = render_text(&font, "12:34", [200.0, 80.0], 10.0, "center", 1).unwrap();
        assert_eq!((w, h), (200, 80));
        // Longer text keeps the font size and widens the picture.
        let (w2, h2, _, _) = render_text(&font, "12:34:56 TUESDAY", [200.0, 80.0], 10.0, "center", 1).unwrap();
        assert!(w2 > 200 && h2 == 80);
        // Two lines in a two-row box: half-height letters, ink in both halves, and narrower than one long line.
        let (w3, h3, two, _) = render_text(&font, "TUES\nDAY", [4.0, 120.0], 10.0, "center", 2).unwrap();
        let (w4, _, _, _) = render_text(&font, "TUESDAY", [4.0, 120.0], 10.0, "center", 2).unwrap();
        assert!(h3 == 120 && w3 < w4);
        let ink_rows: Vec<bool> = two.chunks(w3 as usize * 4).map(|r| r.chunks(4).any(|p| p[3] > 200)).collect();
        assert!(ink_rows[..60].iter().any(|&r| r) && ink_rows[60..].iter().any(|&r| r));
        // More lines than the box was made for grow it downward instead of shrinking.
        let (_, h5, _, _) = render_text(&font, "A\nB\nC", [4.0, 120.0], 10.0, "center", 2).unwrap();
        assert!(h5 > 120);
        // Taller than 4096 px (a vertical date): drawn smaller as a whole, every row kept, with the factor returned.
        let date = "1\n5\n\nN\nO\nV\n\n2\n0\n2\n6";
        let (w6, h6, px6, res) = render_text(&font, date, [374.0, 5300.0], 32.0, "center", 11).unwrap();
        assert!(h6 <= 4096 && (res - 4096.0 / 5300.0).abs() < 0.01, "{w6}x{h6} at {res}");
        // The last row (the year's last digit) has ink: nothing was cut off at the bottom.
        let rows: Vec<bool> = px6.chunks(w6 as usize * 4).map(|r| r.chunks(4).any(|p| p[3] > 128)).collect();
        assert!(rows[rows.len() * 10 / 11..].iter().any(|&r| r), "the bottom row is empty");
        // Some pixels are ink, the corners are empty.
        assert!(px.chunks(4).any(|p| p[3] > 200));
        assert_eq!(px[3], 0);
    }

    #[test]
    fn decodes_dxt1() {
        // One block: colors pure red (c0) and pure blue (c1), every pixel picks c1 except the first, which picks c0.
        let red = 0xF800u16.to_le_bytes();
        let blue = 0x001Fu16.to_le_bytes();
        let block = [red[0], red[1], blue[0], blue[1], 0b0101_0100, 0x55, 0x55, 0x55];
        let px = decode_dxt(&block, 4, 4, 1).unwrap();
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
        assert_eq!(&px[4..8], &[0, 0, 255, 255]);
        assert_eq!(hoist_samplers("#include \"x.h\"\nuniform sampler2D g_Texture0; // {}\nvoid main() {}"), "uniform sampler2D g_Texture0; // {}\n#include \"x.h\"\nvoid main() {}");
    }

    #[test]
    fn reads_shader_notes() {
        let src = "uniform float g_Speed; // {\"material\":\"speed\",\"default\":5}\nuniform sampler2D g_Texture1; // {\"combo\":\"MASK\"}\n// [COMBO] {\"combo\":\"VERTICAL\",\"default\":0}\n#if MASK && VERTICAL\n#endif\n#ifdef HLSL\n#endif\n#if defined(SKIN)\n#endif";
        let u = uniforms(src);
        assert_eq!((u[0].ty.as_str(), u[0].name.as_str(), u[0].note["default"].as_i64()), ("float", "g_Speed", Some(5)));
        assert_eq!(combo_defaults(src).get("VERTICAL"), Some(&0));
        let names = if_names(src);
        assert!(names.contains("MASK") && names.contains("VERTICAL"));
        assert!(!names.contains("HLSL") && !names.contains("SKIN"));
        assert_eq!(rename_calls("x = max(0, c) + v.max(1) + maxValue + pow (a,2.0);"), "x = w_max(0, c) + v.max(1) + maxValue + w_pow (a,2.0);");
        assert_eq!(floats(&serde_json::json!("1 0.5")), vec![1.0, 0.5]);
        assert_eq!(floats(&serde_json::json!({"user": "x", "value": 2})), vec![2.0]);
    }
}

// ---------- As a wallpaper ----------

/// An OpenGL context on a hidden window of our own: OpenGL needs a window even to draw off screen.
unsafe fn hidden_gl() -> Option<glow::Context> {
    use std::ffi::c_void;
    use windows_sys::Win32::{Graphics::OpenGL::*, System::LibraryLoader::*};
    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        DefWindowProcW(hwnd, msg, wp, lp)
    }
    let class = w("WispGL");
    let wc = WNDCLASSW { style: CS_OWNDC, lpfnWndProc: Some(proc), lpszClassName: class.as_ptr(), ..zeroed() };
    RegisterClassW(&wc);
    let hidden = CreateWindowExW(0, class.as_ptr(), null(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), null_mut(), null());
    let dc = GetDC(hidden);
    let pfd = PIXELFORMATDESCRIPTOR {
        nSize: size_of::<PIXELFORMATDESCRIPTOR>() as u16,
        nVersion: 1,
        dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
        iPixelType: PFD_TYPE_RGBA,
        cColorBits: 32,
        ..zeroed()
    };
    SetPixelFormat(dc, ChoosePixelFormat(dc, &pfd), &pfd);
    let ctx = wglCreateContext(dc);
    if ctx.is_null() || wglMakeCurrent(dc, ctx) == 0 {
        return None;
    }
    let opengl32 = LoadLibraryA(c"opengl32.dll".as_ptr() as *const u8);
    Some(glow::Context::from_loader_function_cstr(|name| {
        // Newer functions come from the driver, OpenGL 1.1 ones only from opengl32.dll.
        match wglGetProcAddress(name.as_ptr() as *const u8) {
            Some(f) if !matches!(f as usize, 1 | 2 | 3 | usize::MAX) => f as *const c_void,
            _ => GetProcAddress(opengl32, name.as_ptr() as *const u8).map_or(null(), |f| f as *const c_void),
        }
    }))
}

/// A scene's music while it plays: one mpv per sound object, made when sound turns on and dropped when it turns off,
/// so a silent scene keeps no sound player in memory.
struct SceneAudio {
    defs: Vec<SoundDef>,
    players: Vec<(crate::mpv::Mpv, f32)>,
    applied: (bool, u32),
}

impl SceneAudio {
    fn new(defs: Vec<SoundDef>) -> SceneAudio {
        SceneAudio { defs, players: Vec::new(), applied: (false, 0) }
    }

    /// Follows the sound setting: starts, stops or changes the loudness of the scene's sounds.
    fn follow(&mut self, assets: &Assets, audio: Option<&crate::mpv::Audio>) {
        let (on, volume) = audio.map_or((false, 0), |a| a.get());
        if (on, volume) == self.applied || self.defs.is_empty() {
            return;
        }
        let loud = |v: f32| ((volume as f32 * v).round() as i32).clamp(0, 100).to_string();
        match (on, self.applied.0) {
            (false, _) => self.players.clear(),
            (true, false) => {
                for def in &self.defs {
                    let files: Vec<String> = def.files.iter().filter_map(|f| assets.media_url(f)).collect();
                    let repeat = if def.mode == "single" { "no" } else { "inf" };
                    let shuffle = if def.mode == "random" { "yes" } else { "no" };
                    let options = [("video", "no"), ("audio-fallback-to-null", "yes"), ("ytdl", "no"), ("load-scripts", "no"), ("terminal", "no"), ("loop-playlist", repeat), ("shuffle", shuffle), ("volume", &loud(def.volume))];
                    let Some(m) = crate::mpv::Mpv::new(&options) else { continue };
                    for f in &files {
                        m.command(&["loadfile", f, "append-play"]);
                    }
                    self.players.push((m, def.volume));
                }
            }
            (true, true) => {
                for (m, v) in &self.players {
                    m.set("volume", &loud(*v));
                }
            }
        }
        self.applied = (on, volume);
    }
}

/// Frees what a scene player made when its thread ends (OpenGL context, its windows). Scene players run inside
/// Wisp, so nothing else would: every wallpaper change would leak them.
struct Cleanup(HWND);

impl Drop for Cleanup {
    fn drop(&mut self) {
        use windows_sys::Win32::Graphics::OpenGL::*;
        unsafe {
            let ctx = wglGetCurrentContext();
            if !ctx.is_null() {
                let hidden = WindowFromDC(wglGetCurrentDC());
                wglMakeCurrent(null_mut(), null_mut());
                wglDeleteContext(ctx);
                if !hidden.is_null() {
                    DestroyWindow(hidden);
                }
            }
            DestroyWindow(self.0);
        }
    }
}

/// Scene settings as passed on the command line: a JSON object of name to value, or nothing.
pub fn parse_settings(arg: &str) -> serde_json::Map<String, Value> {
    serde_json::from_str(arg).unwrap_or_default()
}

/// `wisp.exe --scene-shot <folder> <out.png> [settings]`: renders about 1.5 seconds of a scene off screen, saves the
/// last frame, and writes what failed or was skipped next to it (out.txt). For testing many scenes quickly.
pub fn snapshot(folder: &str, out: &str, settings: &str) {
    let report = |text: String| {
        let _ = fs::write(Path::new(out).with_extension("txt"), text);
    };
    let Some(assets) = Assets::open(Path::new(folder)) else { return report("no scene.pkg".into()) };
    let scene = match Scene::load(&assets, &parse_settings(settings)) {
        Ok(s) => s,
        Err(e) => return report(format!("load: {e}")),
    };
    unsafe {
        let Some(gl) = hidden_gl() else { return report("no OpenGL context".into()) };
        let w = 960;
        let h = (w as f32 / (scene.width / scene.height)) as i32;
        let mut renderer = Renderer::new(assets, scene, w as f32);
        let target = new_target(&gl, w, h);
        for _ in 0..45 {
            renderer.paint(&gl, [0, 0, w, h], Some(target.fbo), true);
            std::thread::sleep(Duration::from_millis(33));
        }
        let mut px = vec![0u8; (w * h * 4) as usize];
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.fbo));
        gl.read_pixels(0, 0, w, h, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
        // The frame was drawn rows top first, the way image files store them.
        let _ = image::save_buffer(out, &px, w as u32, h as u32, image::ExtendedColorType::Rgba8);
        let mut text = String::new();
        for s in renderer.skipped() {
            text += &format!("skipped: {s}\n");
        }
        for e in &renderer.errors {
            text += &format!("error: {e}\n");
        }
        report(text);
    }
}

/// What Wisp and a playing scene share: the fit mode, the part of the picture shown and the zoom (see crate::place),
/// which apply on the next frame, and whether its first frame is on screen yet.
pub struct Live {
    pub place: std::sync::Mutex<(String, (f32, f32), f32)>,
    pub ready: std::sync::atomic::AtomicBool,
}

impl Live {
    pub fn new(fit: &str, pan: (f32, f32), zoom: f32) -> Live {
        Live { place: std::sync::Mutex::new((fit.to_string(), pan, zoom)), ready: std::sync::atomic::AtomicBool::new(false) }
    }
}

/// `wisp.exe --scene-wall <folder> <parent window> <fit> <fps> [settings]`: plays a scene inside Wisp's wallpaper
/// window for one screen, until that window goes away. Wisp starts and stops it like an mpv player.
///
/// The desktop only shows DirectX output (classic OpenGL windows stay invisible behind the icons), so the
/// scene is drawn with OpenGL into a texture shared with Direct3D 11 (NV_DX_interop, supported by NVIDIA,
/// AMD and Intel drivers), and Direct3D shows it through a swap chain, like mpv does.
/// `audio`: the sound setting, for the screen that plays sound (None stays silent). `live`: how it sits on the
/// screen, which Wisp can change while it plays, and the flag it sets once its first frame is up.
pub fn run_wallpaper(folder: &str, parent: isize, fps: u32, settings: &str, stop: &std::sync::atomic::AtomicBool, audio: Option<&crate::mpv::Audio>, live: &Live) {
    use std::ffi::c_void;
    use windows::{
        core::Interface,
        Win32::Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    };
    use windows_sys::Win32::Graphics::OpenGL::*;
    let log = |text: &str| {
        let _ = fs::write(env::temp_dir().join("wisp-scene.log"), text);
    };
    let Some(assets) = Assets::open(Path::new(folder)) else { return log("no scene.pkg") };
    let mut scene = match Scene::load(&assets, &parse_settings(settings)) {
        Ok(s) => s,
        Err(e) => return log(&e),
    };
    let parent = parent as HWND;
    unsafe {
        let mut r: RECT = zeroed();
        GetClientRect(parent, &mut r);
        let (sw, sh) = (r.right - r.left, r.bottom - r.top);
        unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        let class = w("WispScene");
        let wc = WNDCLASSW { style: CS_OWNDC, lpfnWndProc: Some(proc), lpszClassName: class.as_ptr(), ..zeroed() };
        RegisterClassW(&wc);

        // 1. The window behind the icons, shown through a Direct3D 11 swap chain.
        let hwnd = CreateWindowExW(0, class.as_ptr(), null(), WS_CHILD | WS_VISIBLE, 0, 0, sw, sh, parent, null_mut(), null_mut(), null());
        if hwnd.is_null() {
            return log("can't create the wallpaper window");
        }
        let _cleanup = Cleanup(hwnd);
        let (mut device, mut context) = (None, None);
        let made = D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        );
        let (Ok(()), Some(device), Some(context)) = (made, device, context) else { return log("no Direct3D 11 device") };
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory2>() else { return log("no DXGI factory") };
        let sample = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: sw as u32,
            Height: sh as u32,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: sample,
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let swap = match factory.CreateSwapChainForHwnd(&device, windows::Win32::Foundation::HWND(hwnd as _), &desc, None, None::<&IDXGIOutput>) {
            Ok(s) => s,
            Err(e) => return log(&format!("swap chain: {e}")),
        };

        // 2. A texture both share: OpenGL draws the scene into it, Direct3D copies it to the screen.
        let tex_desc = D3D11_TEXTURE2D_DESC {
            Width: sw as u32,
            Height: sh as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: sample,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut shared = None;
        if device.CreateTexture2D(&tex_desc, None, Some(&mut shared)).is_err() {
            return log("can't create the shared texture");
        }
        let Some(shared) = shared else { return };

        // 3. OpenGL, on a hidden window of our own.
        let Some(gl) = hidden_gl() else { return log("no OpenGL context") };

        // 4. Link the shared texture into OpenGL.
        type Open = unsafe extern "system" fn(*mut c_void) -> *mut c_void;
        type Register = unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32) -> *mut c_void;
        type Lock = unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32;
        let get = |n: &std::ffi::CStr| wglGetProcAddress(n.as_ptr() as *const u8);
        type Close = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;
        let (Some(open), Some(register), Some(lock), Some(unlock), Some(unregister), Some(close)) =
            (get(c"wglDXOpenDeviceNV"), get(c"wglDXRegisterObjectNV"), get(c"wglDXLockObjectsNV"), get(c"wglDXUnlockObjectsNV"), get(c"wglDXUnregisterObjectNV"), get(c"wglDXCloseDeviceNV"))
        else {
            return log("this graphics driver has no NV_DX_interop");
        };
        let (open, register, lock, unlock): (Open, Register, Lock, Lock) = (std::mem::transmute(open), std::mem::transmute(register), std::mem::transmute(lock), std::mem::transmute(unlock));
        let (unregister, close): (Close, unsafe extern "system" fn(*mut c_void) -> i32) = (std::mem::transmute(unregister), std::mem::transmute(close));
        let interop = open(device.as_raw());
        if interop.is_null() {
            return log("wglDXOpenDeviceNV failed");
        }
        let gl_tex = gl.create_texture().unwrap();
        const WGL_ACCESS_WRITE_DISCARD_NV: u32 = 0x0002;
        let mut object = register(interop, shared.as_raw(), gl_tex.0.get(), glow::TEXTURE_2D, WGL_ACCESS_WRITE_DISCARD_NV);
        if object.is_null() {
            return log("wglDXRegisterObjectNV failed");
        }
        let fbo = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(gl_tex), 0);

        let mut sound = SceneAudio::new(std::mem::take(&mut scene.sounds));
        let mut renderer = Renderer::new(assets, scene, sw.max(sh) as f32);
        // Fit mode, and for Fill the part the user picked and the zoom (see crate::place), read every frame so a
        // change shows at once. Rows run top first here, so place's top-down numbers apply as they are.
        let placed = |r: &Renderer| {
            let (fit, pan, zoom) = live.place.lock().map(|p| p.clone()).unwrap_or_else(|_| ("fill".into(), (0.0, 0.0), 1.0));
            let [l, t, w, h] = crate::place(r.aspect(), sw as f32 / sh as f32, &fit, pan, zoom);
            [(l * sw as f32).round() as i32, (t * sh as f32).round() as i32, (w * sw as f32).round() as i32, (h * sh as f32).round() as i32]
        };
        let mut shown = [0; 4];
        // shortcut: no cap means 60 fps, not the screen's refresh rate (which can be 240); add a setting if wanted.
        let frame = Duration::from_secs_f64(1.0 / fps.clamp(5, 240) as f64);
        let mut next = Instant::now();
        let mut frames = 0u32;
        loop {
            let mut msg: MSG = zeroed();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            // Wisp stops this screen, or its wallpaper window went away (Explorer restarted).
            if stop.load(std::sync::atomic::Ordering::Relaxed) || IsWindow(parent) == 0 {
                unregister(interop, object);
                close(interop);
                return;
            }
            sound.follow(&renderer.assets, audio);
            // A still scene keeps its last frame on screen and draws again only when a text (a clock) changes:
            // no GPU or CPU work, and its memory can stay out of RAM.
            let still = frames >= 2 && !renderer.moving;
            let viewport = placed(&renderer);
            let moved = viewport != shown;
            shown = viewport;
            if still && !moved && !renderer.refresh_texts() {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            lock(interop, 1, &mut object);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, sw, sh);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            renderer.paint(&gl, viewport, Some(fbo), true);
            frames += 1;
            if frames == 120 || (frames == 2 && !renderer.moving) {
                // A note on how it went, for when a scene shows wrong or black.
                let mut px = [0u8; 4];
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.read_pixels(sw / 2, sh / 2, 1, 1, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
                let state = if renderer.moving { "moving" } else { "still" };
                let mut text = format!("OpenGL {}, shown with Direct3D 11\nviewport {viewport:?} window {sw}x{sh}\ncenter pixel {px:?}\n{state}\n", gl.get_parameter_string(glow::VERSION));
                for s in renderer.skipped() {
                    text += &format!("skipped: {s}\n");
                }
                for e in &renderer.errors {
                    text += &format!("error: {e}\n");
                }
                log(&text);
            }
            gl.flush();
            unlock(interop, 1, &mut object);
            if let Ok(back) = swap.GetBuffer::<ID3D11Texture2D>(0) {
                context.CopyResource(&back, &shared);
            }
            let _ = swap.Present(0, DXGI_PRESENT(0));
            // The first frame is up: Wisp can take away the wallpaper this one replaces.
            live.ready.store(true, std::sync::atomic::Ordering::Relaxed);
            // Loading is done: give Windows back the memory only loading used (decoding, shader compiling).
            // A still scene touches nothing more, so it stays small; a moving one keeps only what each frame needs.
            if frames == 120 || (frames == 2 && !renderer.moving) || still {
                windows_sys::Win32::System::Threading::SetProcessWorkingSetSize(windows_sys::Win32::System::Threading::GetCurrentProcess(), usize::MAX, usize::MAX);
            }
            next += frame;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    }
}

// ---------- Particles ----------

#[derive(Clone, Copy)]
struct Range {
    min: f32,
    max: f32,
}

impl Range {
    fn of(v: &Value, min: f32, max: f32) -> Range {
        Range { min: floats(&v["min"]).first().copied().unwrap_or(min), max: floats(&v["max"]).first().copied().unwrap_or(max) }
    }
    fn pick(&self, r: &mut Rng) -> f32 {
        self.min + (self.max - self.min) * r.next()
    }
}

/// Small fast random numbers; particles need many and nothing secret.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn vec2_of(v: &Value, default: [f32; 2]) -> [f32; 2] {
    let f = floats(v);
    match f.len() {
        0 => default,
        1 => [f[0], f[0]],
        _ => [f[0], f[1]],
    }
}

fn vec3_of(v: &Value, default: [f32; 3]) -> [f32; 3] {
    let f = floats(v);
    if f.len() >= 3 { [f[0], f[1], f[2]] } else { default }
}

struct Emitter {
    rate: f32,
    origin: [f32; 2],
    /// Sphere: distance from the origin. Box: half size of the box.
    dist_min: [f32; 2],
    dist_max: [f32; 2],
    directions: [f32; 2],
    boxed: bool,
}

/// Over a particle's life (0..1), from start value at `t0` to end value at `t1`.
#[derive(Clone, Copy)]
struct Change {
    t0: f32,
    t1: f32,
}

impl Change {
    fn of(v: &Value) -> Change {
        Change { t0: floats(&v["starttime"]).first().copied().unwrap_or(0.0), t1: floats(&v["endtime"]).first().copied().unwrap_or(1.0) }
    }
    fn at(&self, t: f32) -> f32 {
        ((t - self.t0) / (self.t1 - self.t0).max(1e-4)).clamp(0.0, 1.0)
    }
}

enum Op {
    Movement { gravity: [f32; 2], drag: f32 },
    AlphaFade { fade_in: f32, fade_out: f32 },
    Size { when: Change, from: f32, to: f32 },
    Alpha { when: Change, from: f32, to: f32 },
    Color { when: Change, from: [f32; 3], to: [f32; 3] },
    Oscillate { mask: [f32; 2], freq: Range, scale: Range, phase: Range },
    Turbulence { scale: f32, speed: Range, timescale: f32 },
}

/// A particle system as its JSON describes it.
struct ParticleDef {
    emitters: Vec<Emitter>,
    max: usize,
    life: Range,
    size: Range,
    size_exponent: f32,
    rotation: Range,
    spin: Range,
    vel_min: [f32; 2],
    vel_max: [f32; 2],
    swirl: Option<Range>,
    color_min: [f32; 3],
    color_max: [f32; 3],
    alpha: Range,
    ops: Vec<Op>,
    texture: Option<String>,
    additive: bool,
    random_frame: bool,
    prewarm: f32,
}

fn load_particles(assets: &Assets, file: &str, skipped: &mut Vec<String>) -> Vec<ParticleDef> {
    let Some(j) = assets.json(file) else {
        skipped.push(format!("missing particles {file}"));
        return Vec::new();
    };
    let mut def = ParticleDef {
        emitters: Vec::new(),
        max: j["maxcount"].as_u64().unwrap_or(1000).min(50_000) as usize,
        life: Range { min: 1.0, max: 1.0 },
        size: Range { min: 20.0, max: 20.0 },
        size_exponent: 1.0,
        rotation: Range { min: 0.0, max: 0.0 },
        spin: Range { min: 0.0, max: 0.0 },
        vel_min: [0.0; 2],
        vel_max: [0.0; 2],
        swirl: None,
        color_min: [1.0; 3],
        color_max: [1.0; 3],
        alpha: Range { min: 1.0, max: 1.0 },
        ops: Vec::new(),
        texture: None,
        additive: false,
        random_frame: j["animationmode"].as_str() == Some("randomframe"),
        prewarm: floats(&j["starttime"]).first().copied().unwrap_or(0.0).min(30.0),
    };
    for e in j["emitter"].as_array().into_iter().flatten() {
        let boxed = e["name"].as_str() == Some("boxrandom");
        def.emitters.push(Emitter {
            rate: floats(&e["rate"]).first().copied().unwrap_or(5.0),
            origin: vec2_of(&e["origin"], [0.0; 2]),
            dist_min: vec2_of(&e["distancemin"], [0.0; 2]),
            dist_max: vec2_of(&e["distancemax"], if boxed { [0.0; 2] } else { [256.0; 2] }),
            directions: vec2_of(&e["directions"], [1.0, 1.0]),
            boxed,
        });
    }
    for i in j["initializer"].as_array().into_iter().flatten() {
        match i["name"].as_str().unwrap_or("") {
            "lifetimerandom" => def.life = Range::of(i, 1.0, 1.0),
            "sizerandom" => {
                def.size = Range::of(i, 0.0, 20.0);
                def.size_exponent = floats(&i["exponent"]).first().copied().unwrap_or(1.0);
            }
            "rotationrandom" => {
                // Rotation is around z: the last number when given as "x y z".
                let pick = |v: &Value, d: f32| floats(v).last().copied().unwrap_or(d);
                def.rotation = Range { min: pick(&i["min"], 0.0), max: pick(&i["max"], std::f32::consts::TAU) };
            }
            "angularvelocityrandom" => {
                let pick = |v: &Value, d: f32| floats(v).last().copied().unwrap_or(d);
                def.spin = Range { min: pick(&i["min"], -5.0), max: pick(&i["max"], 5.0) };
            }
            "velocityrandom" => {
                def.vel_min = vec2_of(&i["min"], [-32.0; 2]);
                def.vel_max = vec2_of(&i["max"], [32.0; 2]);
            }
            "turbulentvelocityrandom" => {
                def.swirl = Some(Range { min: floats(&i["speedmin"]).first().copied().unwrap_or(100.0), max: floats(&i["speedmax"]).first().copied().unwrap_or(250.0) })
            }
            "colorrandom" => {
                def.color_min = vec3_of(&i["min"], [255.0; 3]).map(|c| c / 255.0);
                def.color_max = vec3_of(&i["max"], [255.0; 3]).map(|c| c / 255.0);
            }
            "alpharandom" => def.alpha = Range::of(i, 1.0, 1.0),
            other => skipped.push(format!("particle start value {other}")),
        }
    }
    for o in j["operator"].as_array().into_iter().flatten() {
        let f = |k: &str, d: f32| floats(&o[k]).first().copied().unwrap_or(d);
        let op = match o["name"].as_str().unwrap_or("") {
            "movement" => Op::Movement { gravity: vec2_of(&o["gravity"], [0.0; 2]), drag: f("drag", 0.0) },
            "alphafade" => Op::AlphaFade { fade_in: f("fadeintime", 0.5), fade_out: f("fadeouttime", 0.5) },
            "sizechange" => Op::Size { when: Change::of(o), from: f("startvalue", 1.0), to: f("endvalue", 0.0) },
            "alphachange" => Op::Alpha { when: Change::of(o), from: f("startvalue", 1.0), to: f("endvalue", 0.0) },
            "colorchange" => Op::Color { when: Change::of(o), from: vec3_of(&o["startvalue"], [1.0; 3]), to: vec3_of(&o["endvalue"], [1.0; 3]) },
            "oscillateposition" => Op::Oscillate {
                mask: vec2_of(&o["mask"], [1.0, 1.0]),
                freq: Range { min: f("frequencymin", 0.0), max: f("frequencymax", 5.0) },
                scale: Range { min: f("scalemin", 0.0), max: f("scalemax", 1.0) },
                phase: Range { min: f("phasemin", 0.0), max: f("phasemax", std::f32::consts::TAU) },
            },
            "turbulence" => Op::Turbulence { scale: f("scale", 0.005), speed: Range { min: f("speedmin", 500.0), max: f("speedmax", 1000.0) }, timescale: f("timescale", 1.0) },
            "angularmovement" => continue,
            other => {
                skipped.push(format!("particle operator {other}"));
                continue;
            }
        };
        def.ops.push(op);
    }
    if let Some(m) = j["material"].as_str().and_then(|m| assets.json(m)) {
        let pass = &m["passes"][0];
        def.texture = pass["textures"][0].as_str().map(String::from);
        def.additive = pass["blending"].as_str() == Some("additive");
    }
    let mut all = vec![def];
    // Child systems (like a torch's embers) play at the same spot.
    for child in j["children"].as_array().into_iter().flatten() {
        if let Some(name) = child["name"].as_str() {
            all.extend(load_particles(assets, name, skipped));
        }
    }
    all
}

struct Particle {
    pos: [f32; 2],
    vel: [f32; 2],
    age: f32,
    life: f32,
    size: f32,
    rot: f32,
    spin: f32,
    color: [f32; 3],
    alpha: f32,
    frame: usize,
    /// Per particle oscillation: frequency, scale, phase.
    osc: [f32; 3],
}

/// One particle system placed in the scene, with the scene's overrides.
pub struct System {
    order: usize,
    origin: [f32; 2],
    scale: [f32; 2],
    angle: f32,
    def: ParticleDef,
    parts: Vec<Particle>,
    pending: f32,
    rng: Rng,
    rate: f32,
    lifetime: f32,
    speed: f32,
    size: f32,
    alpha: f32,
    color: [f32; 3],
    max: usize,
}

impl System {
    /// `place` is the system's center, scale and angle in the scene, after its parents.
    fn new(order: usize, obj: &Value, def: ParticleDef, seed: u64, place: ([f32; 2], [f32; 2], f32)) -> System {
        let o = &obj["instanceoverride"];
        let f = |k: &str| floats(&o[k]).first().copied().unwrap_or(1.0);
        let max = ((def.max as f32) * f("count")).ceil().max(1.0) as usize;
        System {
            order,
            origin: place.0,
            scale: place.1,
            angle: place.2,
            def,
            parts: Vec::new(),
            pending: 0.0,
            rng: Rng(seed | 1),
            rate: f("rate"),
            lifetime: f("lifetime"),
            speed: f("speed"),
            size: f("size"),
            alpha: f("alpha"),
            color: vec3_of(&o["colorn"], [1.0; 3]),
            max,
        }
    }

    fn spawn(&mut self, frames: usize) {
        let r = &mut self.rng;
        let d = &self.def;
        let Some(e) = d.emitters.get((r.next() * d.emitters.len() as f32) as usize % d.emitters.len().max(1)) else { return };
        let pos = if e.boxed {
            let p = |i: usize, r: &mut Rng| {
                let v = e.dist_min[i] + (e.dist_max[i] - e.dist_min[i]) * r.next();
                if r.next() < 0.5 { -v } else { v }
            };
            [e.origin[0] + p(0, r), e.origin[1] + p(1, r)]
        } else {
            let a = r.next() * std::f32::consts::TAU;
            let dist = e.dist_min[0] + (e.dist_max[0] - e.dist_min[0]) * r.next();
            [e.origin[0] + a.cos() * dist * e.directions[0], e.origin[1] + a.sin() * dist * e.directions[1]]
        };
        let mut vel = [d.vel_min[0] + (d.vel_max[0] - d.vel_min[0]) * r.next(), d.vel_min[1] + (d.vel_max[1] - d.vel_min[1]) * r.next()];
        if let Some(s) = d.swirl {
            let (a, v) = (r.next() * std::f32::consts::TAU, s.pick(r));
            vel = [vel[0] + a.cos() * v, vel[1] + a.sin() * v];
        }
        let size = d.size.min + (d.size.max - d.size.min) * r.next().powf(d.size_exponent.max(0.01));
        let mix = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let t = [r.next(), r.next(), r.next()];
        let mut osc = [0.0; 3];
        for op in &d.ops {
            if let Op::Oscillate { freq, scale, phase, .. } = op {
                osc = [freq.pick(r), scale.pick(r), phase.pick(r)];
            }
        }
        self.parts.push(Particle {
            pos,
            vel: vel.map(|v| v * self.speed),
            age: 0.0,
            life: d.life.pick(r).max(0.01) * self.lifetime,
            size,
            rot: d.rotation.pick(r),
            spin: d.spin.pick(r),
            color: [mix(d.color_min[0], d.color_max[0], t[0]), mix(d.color_min[1], d.color_max[1], t[1]), mix(d.color_min[2], d.color_max[2], t[2])],
            alpha: d.alpha.pick(r),
            frame: if d.random_frame && frames > 1 { (r.next() * frames as f32) as usize % frames } else { 0 },
            osc,
        });
    }

    fn step(&mut self, dt: f32, time: f32, frames: usize) {
        let rate: f32 = self.def.emitters.iter().map(|e| e.rate).sum::<f32>() * self.rate;
        self.pending = (self.pending + rate * dt).min(self.max as f32);
        while self.pending >= 1.0 && self.parts.len() < self.max {
            self.pending -= 1.0;
            self.spawn(frames);
        }
        if self.parts.len() >= self.max {
            self.pending = 0.0;
        }
        let ops = &self.def.ops;
        self.parts.retain_mut(|p| {
            p.age += dt;
            if p.age >= p.life {
                return false;
            }
            for op in ops {
                match op {
                    Op::Movement { gravity, drag } => {
                        p.vel = [p.vel[0] + gravity[0] * dt, p.vel[1] + gravity[1] * dt];
                        let keep = (1.0 - drag * dt).max(0.0);
                        p.vel = p.vel.map(|v| v * keep);
                    }
                    Op::Turbulence { scale, speed, timescale } => {
                        // A cheap swirling field instead of real noise: same look of drifting curls.
                        let a = ((p.pos[0] * scale * 6.0 + time * timescale * 0.1).sin() + (p.pos[1] * scale * 6.0 - time * timescale * 0.07).cos()) * std::f32::consts::PI;
                        let s = (speed.min + speed.max) * 0.5 * dt;
                        p.vel = [p.vel[0] + a.cos() * s, p.vel[1] + a.sin() * s];
                    }
                    _ => {}
                }
            }
            if ops.iter().any(|o| matches!(o, Op::Movement { .. })) {
                p.pos = [p.pos[0] + p.vel[0] * dt, p.pos[1] + p.vel[1] * dt];
            }
            p.rot += p.spin * dt;
            true
        });
    }

    /// Adds this system's particles as triangles: clip position (rows top first), texture position, color.
    fn vertices(&self, out: &mut Vec<f32>, frames: &[[f32; 4]], scene_w: f32, scene_h: f32) {
        let (ca, sa) = (self.angle.cos(), self.angle.sin());
        for p in &self.parts {
            let t = p.age / p.life;
            let (mut size, mut alpha, mut color, mut offset) = (p.size * self.size, p.alpha * self.alpha, p.color, [0.0f32; 2]);
            for op in &self.def.ops {
                match op {
                    Op::AlphaFade { fade_in, fade_out } => {
                        if t < *fade_in {
                            alpha *= t / fade_in.max(1e-4);
                        }
                        if t > *fade_out {
                            alpha *= 1.0 - (t - fade_out) / (1.0 - fade_out).max(1e-4);
                        }
                    }
                    Op::Size { when, from, to } => size *= from + (to - from) * when.at(t),
                    Op::Alpha { when, from, to } => alpha *= from + (to - from) * when.at(t),
                    Op::Color { when, from, to } => {
                        let k = when.at(t);
                        color = [0, 1, 2].map(|i| color[i] * (from[i] + (to[i] - from[i]) * k));
                    }
                    Op::Oscillate { mask, .. } => {
                        let s = (std::f32::consts::TAU * p.osc[0] * p.age + p.osc[2]).sin() * p.osc[1];
                        offset = [mask[0] * s, mask[1] * s];
                    }
                    _ => {}
                }
            }
            if alpha <= 0.001 || size <= 0.0 {
                continue;
            }
            color = [0, 1, 2].map(|i| color[i] * self.color[i]);
            // Particle space to scene space: the system's scale, rotation and place.
            let local = [(p.pos[0] + offset[0]) * self.scale[0], (p.pos[1] + offset[1]) * self.scale[1]];
            let center = [self.origin[0] + local[0] * ca - local[1] * sa, self.origin[1] + local[0] * sa + local[1] * ca];
            let half = size * 0.5 * self.scale[0];
            let (cr, sr) = ((p.rot + self.angle).cos(), (p.rot + self.angle).sin());
            let uv = frames.get(p.frame).copied().unwrap_or([0.0, 0.0, 1.0, 1.0]);
            let corner = |dx: f32, dy: f32, u: f32, v: f32, out: &mut Vec<f32>| {
                let x = center[0] + (dx * cr - dy * sr) * half;
                let y = center[1] + (dx * sr + dy * cr) * half;
                out.extend_from_slice(&[2.0 * x / scene_w - 1.0, 1.0 - 2.0 * y / scene_h, uv[0] + u * uv[2], uv[1] + v * uv[3], color[0], color[1], color[2], alpha]);
            };
            for (dx, dy, u, v) in [(-1.0, -1.0, 0.0, 1.0), (1.0, -1.0, 1.0, 1.0), (1.0, 1.0, 1.0, 0.0), (-1.0, -1.0, 0.0, 1.0), (1.0, 1.0, 1.0, 0.0), (-1.0, 1.0, 0.0, 0.0)] {
                corner(dx, dy, u, v, out);
            }
        }
    }
}

const PARTICLE_VERT: &str = "#version 120
attribute vec2 a_Pos; attribute vec2 a_UV; attribute vec4 a_Color; varying vec2 v_UV; varying vec4 v_Color;
void main() { v_UV = a_UV; v_Color = a_Color; gl_Position = vec4(a_Pos, 0.0, 1.0); }";
/// u_Mode: 0 color texture, 1 one-channel texture (its value is the alpha), 2 two-channel (gray and alpha).
const PARTICLE_FRAG: &str = "#version 120
varying vec2 v_UV; varying vec4 v_Color; uniform sampler2D u_Tex; uniform int u_Mode;
void main() {
    vec4 t = texture2D(u_Tex, v_UV);
    if (u_Mode == 1) t = vec4(1.0, 1.0, 1.0, t.r);
    else if (u_Mode == 2) t = vec4(t.rrr, t.g);
    else if (u_Mode == 3) t = vec4(1.0, 1.0, 1.0, max(0.0, 1.0 - length(v_UV * 2.0 - 1.0)));
    gl_FragColor = t * v_Color;
}";

// ---------- Text ----------

/// What a scene script (SceneScript, Wallpaper Engine's JavaScript) can call, stubbed: settings builders,
/// the engine object, layer and storage. Clock and date scripts need little beyond `new Date()`.
const SCRIPT_PRELUDE: &str = r#"
function createScriptProperties() {
    const p = {};
    const b = {
        addCheckbox(o) { p[o.name] = o.value; return b; },
        addText(o) { p[o.name] = o.value; return b; },
        addSlider(o) { p[o.name] = o.value; return b; },
        addColor(o) { p[o.name] = o.value; return b; },
        addFile(o) { p[o.name] = o.value; return b; },
        addDirectory(o) { p[o.name] = o.value; return b; },
        addCombo(o) { p[o.name] = o.value !== undefined ? o.value : (o.options && o.options[0] ? o.options[0].value : undefined); return b; },
        finish() { return p; }
    };
    return b;
}
const anything = new Proxy({}, { get: (t, k) => k in t ? t[k] : (() => 0) });
var engine = new Proxy({ frametime: 0.016, runtime: 0, timeOfDay: 0, screenResolution: { x: 1920, y: 1080 }, canvasSize: { x: 1920, y: 1080 },
    registerAudioBuffers() { return { left: [], right: [], average: [] }; }, isDesktopDevice() { return true; }, isWallpaper() { return true; },
    isScreensaver() { return false; }, isRunningInEditor() { return false; } }, { get: (t, k) => k in t ? t[k] : (() => 0) });
var thisLayer = new Proxy({ text: '' }, { get: (t, k) => k in t ? t[k] : (() => 0) });
var thisScene = anything, shared = {}, input = anything, console = { log() {}, error() {}, warn() {} };
var localStorage = { get() { return undefined; }, set() {}, remove() {}, clear() {} };
"#;

/// Turns an ES module scene script into a plain script: imports dropped, `export` removed, so its functions
/// become globals Wisp can call.
fn prepare_script(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("import "))
        .map(|l| {
            let t = l.trim_start();
            match t.strip_prefix("export default ").or_else(|| t.strip_prefix("export ")) {
                Some(rest) => format!("{}{rest}", &l[..l.len() - t.len()]),
                None => l.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// How long one script call may run. Stubbed engine values never change, so a script waiting on them would loop forever.
const SCRIPT_TIME: Duration = Duration::from_millis(50);

/// A text object while it plays: its script (if any) and the text currently shown.
struct TextRun {
    def: TextDef,
    js: Option<(rquickjs::Runtime, rquickjs::Context)>,
    /// When the running script call is stopped; QuickJS checks it while the script runs.
    deadline: Arc<Mutex<Instant>>,
    /// Why the script failed, for the error list (the placeholder text is shown instead).
    error: Option<String>,
    shown: Option<String>,
    /// New text waiting to be drawn.
    want: Option<String>,
    next: Instant,
}

/// The message of the exception a failed QuickJS call left behind.
fn js_error(c: &rquickjs::Ctx, e: rquickjs::Error) -> String {
    match e {
        rquickjs::Error::Exception => c.catch().get::<rquickjs::Coerced<String>>().map_or_else(|_| "script error".into(), |s| s.0),
        e => e.to_string(),
    }
}

impl TextRun {
    fn new(def: TextDef) -> TextRun {
        let deadline = Arc::new(Mutex::new(Instant::now() + SCRIPT_TIME));
        let mut error = None;
        let js = def.script.as_deref().and_then(|src| {
            let rt = rquickjs::Runtime::new().ok()?;
            rt.set_memory_limit(16 << 20);
            let d = deadline.clone();
            rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > *d.lock().unwrap())));
            let ctx = rquickjs::Context::full(&rt).ok()?;
            let loaded: Result<(), String> = ctx.with(|c| {
                c.eval::<(), _>(SCRIPT_PRELUDE).map_err(|e| js_error(&c, e))?;
                c.eval::<(), _>(prepare_script(src)).map_err(|e| js_error(&c, e))?;
                // Settings the scene changed (12-hour clock, date format...) replace the script's defaults.
                for (k, v) in &def.props {
                    let v = v.get("value").unwrap_or(v);
                    let _ = c.eval::<(), _>(format!("if (typeof scriptProperties === 'object') scriptProperties[{}] = {};", Value::String(k.clone()), v));
                }
                Ok(())
            });
            match loaded {
                Ok(()) => Some((rt, ctx)),
                Err(e) => {
                    error = Some(format!("text script: {e}"));
                    None
                }
            }
        });
        TextRun { def, js, deadline, error, shown: None, want: None, next: Instant::now() }
    }

    /// The text to show now: the script's update() result, or the text shown so far if it has none or fails.
    fn value(&mut self) -> String {
        let current = self.shown.clone().unwrap_or_else(|| self.def.value.clone());
        let Some((_, ctx)) = &self.js else { return current };
        *self.deadline.lock().unwrap() = Instant::now() + SCRIPT_TIME;
        // eval, not globals(): `export const update = ...` is a top-level const, which is not on the global object.
        let got = ctx.with(|c| {
            // Scripts without update() (song titles that wait for media events) keep their text.
            let f: Option<rquickjs::Function> = c.eval("typeof update === 'function' ? update : undefined").map_err(|e| js_error(&c, e))?;
            let Some(f) = f else { return Ok(current.clone()) };
            f.call::<_, rquickjs::Coerced<String>>((current.clone(),)).map(|s| s.0).map_err(|e| js_error(&c, e))
        });
        got.unwrap_or_else(|e| {
            self.error = Some(format!("text script: {e}"));
            current
        })
    }
}

/// Passes are drawn through a y-flipping projection so buffers keep rows top first. Some effect shaders skip the
/// projection (`gl_Position = vec4(a_Position, 1.0)`), which would turn their result upside down; flip those too.
fn flip_unprojected(vert: &str) -> String {
    vert.lines()
        .map(|line| {
            let t = line.trim_start();
            // Only positions taken from the quad; composelayer builds its own from texture coordinates and is right as is.
            let direct = t.starts_with("gl_Position") && t[11..].trim_start().starts_with('=') && t.contains("a_Position") && !t.contains("mul(") && t.ends_with(';');
            if direct { format!("{line} gl_Position.y = -gl_Position.y;") } else { line.to_string() }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `float m = texSample2D(...);` is valid in Wallpaper Engine (DirectX keeps the first channel) but an error in
/// OpenGL: make the first channel explicit. Only whole-line declarations whose value is exactly one sample.
fn first_channel(frag: &str) -> String {
    frag.lines()
        .map(|line| {
            let t = line.trim();
            let call = t.strip_prefix("float ").and_then(|r| r.split_once('=')).map(|(_, v)| v.trim()).and_then(|v| v.strip_suffix(';'));
            match call {
                // The sample's closing parenthesis must be the last one, so `texSample2D(a).x * 2.0` etc. stay as they are.
                Some(v) if (v.starts_with("texSample2D(") || v.starts_with("texSample2DLod(")) && v.ends_with(')') && closes_at_end(v) => {
                    format!("{}.x;", line.trim_end().trim_end_matches(';'))
                }
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when the first "(" in `s` is closed by its last character.
fn closes_at_end(s: &str) -> bool {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i == s.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// Draws text into a white picture with soft edges (alpha).
/// The saved box was sized by Wallpaper Engine around the placeholder text: its height inside the padding is the
/// line height (`rows` lines of the placeholder), so that sets the font size. Longer text ("TUESDAY" vs "DAY") or more
/// lines grow the picture instead of shrinking the letters. Returns (width, height, pixels, resolution): a text
/// bigger than 4096 px is drawn smaller as a whole, and `resolution` (below 1) says by how much, so its layer keeps
/// its real size (a tall vertical date was cut off at 4096 before).
fn render_text(font: &[u8], text: &str, box_size: [f32; 2], padding: f32, align: &str, rows: usize) -> Option<(u32, u32, Vec<u8>, f32)> {
    use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
    let data = font;
    let font = FontRef::try_from_slice(data).ok()?;
    let px = ((box_size[1] - 2.0 * padding) / rows.max(1) as f32).max(4.0);
    let sf = font.as_scaled(PxScale::from(px));
    let lines: Vec<&str> = text.lines().collect();
    let width = |line: &str| {
        let mut x = 0.0;
        let mut prev = None;
        for ch in line.chars() {
            let id = sf.glyph_id(ch);
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            x += sf.h_advance(id);
            prev = Some(id);
        }
        x
    };
    let widest = lines.iter().map(|l| width(l)).fold(0.0, f32::max);
    let block = px * lines.len() as f32;
    let (need_w, need_h) = (box_size[0].max(widest + 2.0 * padding), box_size[1].max(block + 2.0 * padding));
    let fit = 4096.0 / need_w.max(need_h);
    if fit < 0.999 {
        let small = render_text(data, text, [box_size[0] * fit, box_size[1] * fit], padding * fit, align, rows)?;
        return Some((small.0, small.1, small.2, fit));
    }
    let w = need_w.clamp(4.0, 4096.0).ceil() as u32;
    let h = need_h.clamp(4.0, 4096.0).ceil() as u32;
    // ab_glyph's pixel size is ascent to descent, so lines sit `px` apart; the block is centered vertically.
    let top = (h as f32 - block) / 2.0;
    let mut alpha = vec![0f32; (w * h) as usize];
    for (row, line) in lines.iter().enumerate() {
        let tw = width(line);
        let mut x = match align {
            "left" => padding,
            "right" => w as f32 - padding - tw,
            _ => (w as f32 - tw) / 2.0,
        };
        let baseline = top + row as f32 * px + sf.ascent();
        let mut prev = None;
        for ch in line.chars() {
            let id = sf.glyph_id(ch);
            if let Some(p) = prev {
                x += sf.kern(p, id);
            }
            if let Some(outline) = font.outline_glyph(id.with_scale_and_position(PxScale::from(px), point(x, baseline))) {
                let b = outline.px_bounds();
                outline.draw(|gx, gy, c| {
                    let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                    if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                        let a = &mut alpha[(py as u32 * w + px as u32) as usize];
                        *a = a.max(c);
                    }
                });
            }
            x += sf.h_advance(id);
            prev = Some(id);
        }
    }
    Some((w, h, alpha.iter().flat_map(|a| [255, 255, 255, (a.clamp(0.0, 1.0) * 255.0) as u8]).collect(), 1.0))
}
