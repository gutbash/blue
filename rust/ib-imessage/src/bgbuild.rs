//! Build chat-background payloads from the templates in `assets/bg/`.
//!
//! The templates are iPhone captures (2026-10-28, see `make_fixtures.py` in
//! the report) with every UUID replaced by a placeholder
//! `00000000-0000-4000-8000-00000000000N` and, for the photo, the image
//! replaced by a neutral generated one. Each build swaps the placeholders
//! for fresh UUIDs (names and contents; all are 36-byte ASCII, so binary
//! plists keep their offsets) and rezips both levels in the captured
//! order, with the captured method per entry (directories stored, files
//! deflated) and Unix permissions.
//!
//! - **photo** (`com.apple.PhotosUIPrivate.PhotosPosterProvider`): the
//!   portrait layer `portrait-layer_background.HEIC` (1292x2796) is
//!   replaced; every frame in `Contents.json` refers to that layer size, so
//!   frames are kept and only `clockAreaLuminance` is recomputed. The watch
//!   preview (820x1003 PNG), `luminance` and `isHighKey` are recomputed.
//!   Image work runs in `prep_photo.py` (Pillow + pillow-heif), because no
//!   Rust HEIC encoder is available here.
//! - **gradient** (`…GradientExtension`): `userInfo.custom` =
//!   `r/g/b/a//r/g/b/a` (six decimals, as captured); the watch preview is a
//!   rendered vertical gradient [VERIFY direction]; `luminance` 0.2 as in
//!   every captured gradient/dynamic background.
//! - **dynamic** (`…DynamicExtension`): `userInfo.identifier` = the preset;
//!   only the three captured presets are offered, each with Apple's own
//!   preview image.

use std::io::{Cursor, Read, Write};

use anyhow::{anyhow, bail, Context, Result};
use plist::Value;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const PHOTO_POSTER: &[u8] = include_bytes!("../assets/bg/photo-poster.zip");
const PHOTO_WATCH: &[u8] = include_bytes!("../assets/bg/photo-watch.plist");
const SIMPLE_POSTER: &[u8] = include_bytes!("../assets/bg/simple-poster.zip");
pub const PREP_PHOTO_PY: &str = include_str!("../assets/bg/prep_photo.py");

pub const PHOTO_EXT: &str = "com.apple.PhotosUIPrivate.PhotosPosterProvider";
pub const GRADIENT_EXT: &str = "com.apple.transcriptBackgroundPoster.GradientExtension";
pub const DYNAMIC_EXT: &str = "com.apple.transcriptBackgroundPoster.DynamicExtension";

pub const DYNAMIC_PRESETS: &[(&str, &[u8])] = &[
    ("aurora_2", include_bytes!("../assets/bg/dynamic-aurora_2.png")),
    ("clouds_2", include_bytes!("../assets/bg/dynamic-clouds_2.png")),
    ("ocean_2", include_bytes!("../assets/bg/dynamic-ocean_2.png")),
];

const USER_INFO: &str = "configuration/versions/0/contents/com.apple.posterkit.provider.contents.userInfo";
const HEIC_SUFFIX: &str = "output.layerStack/portrait-layer_background.HEIC";
const CONTENTS_JSON: &str = "output.layerStack/Contents.json";
const WATCH_SIZE: (u32, u32) = (820, 1003);

pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub stored: bool,
    pub mode: Option<u32>,
    pub data: Vec<u8>,
}

pub fn read_zip(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut z = ZipArchive::new(Cursor::new(bytes)).context("reading zip")?;
    let mut out = Vec::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        out.push(Entry {
            name: f.name().to_string(),
            dir: f.is_dir(),
            stored: f.compression() == CompressionMethod::Stored,
            mode: f.unix_mode(),
            data,
        });
    }
    Ok(out)
}

pub fn write_zip(entries: &[Entry]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut w = ZipWriter::new(Cursor::new(&mut buf));
        for e in entries {
            let method = if e.stored { CompressionMethod::Stored } else { CompressionMethod::Deflated };
            let mut opts = SimpleFileOptions::default().compression_method(method);
            if let Some(m) = e.mode {
                opts = opts.unix_permissions(m & 0o777);
            }
            if e.dir {
                w.add_directory(e.name.trim_end_matches('/'), opts)?;
            } else {
                w.start_file(e.name.as_str(), opts)?;
                w.write_all(&e.data)?;
            }
        }
        w.finish()?;
    }
    Ok(buf)
}

fn replace_all(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() || hay.len() < from.len() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

pub fn placeholder(n: usize) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

/// Swap every placeholder UUID for a fresh one, in names and contents.
fn fresh_uuids(entries: &mut [Entry]) {
    for n in 1..=9 {
        let ph = placeholder(n);
        let fresh = uuid::Uuid::new_v4().to_string().to_uppercase();
        for e in entries.iter_mut() {
            if e.name.contains(&ph) {
                e.name = e.name.replace(&ph, &fresh);
            }
            if !e.name.ends_with(".HEIC") {
                e.data = replace_all(&e.data, ph.as_bytes(), fresh.as_bytes());
            }
        }
    }
}

fn bplist(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    v.to_writer_binary(&mut out)?;
    Ok(out)
}

/// The outer payload: `transcriptBackground/watchBackground` then
/// `transcriptBackground/poster`, both deflated, as captured.
fn outer(watch: &Value, poster: Vec<u8>) -> Result<Vec<u8>> {
    write_zip(&[
        Entry { name: "transcriptBackground/watchBackground".into(), dir: false, stored: false, mode: Some(0o644), data: bplist(watch)? },
        Entry { name: "transcriptBackground/poster".into(), dir: false, stored: false, mode: Some(0o644), data: poster },
    ])
}

fn watch_dict(is_high_key: bool, luminance: f64, png: Vec<u8>, ext: &str) -> Value {
    let mut d = plist::Dictionary::new();
    d.insert("isHighKey".into(), Value::Boolean(is_high_key));
    d.insert("luminance".into(), Value::Real(luminance));
    d.insert("backgroundImageData".into(), Value::Data(png));
    d.insert("extensionIdentifier".into(), Value::String(ext.into()));
    Value::Dictionary(d)
}

/// Values the photo prep derives from the image.
#[derive(serde::Deserialize, Debug, Clone, Copy)]
pub struct Derived {
    pub luminance: f64,
    pub clock_luminance: f64,
    pub is_high_key: bool,
}

/// Replace `"clockAreaLuminance" : <n>` in Contents.json without
/// re-serialising, so key order and Apple's formatting stay as captured.
fn set_json_number(text: &str, key: &str, value: f64) -> Result<String> {
    let pat = format!("\"{key}\" : ");
    let at = text.find(&pat).ok_or_else(|| anyhow!("{key} not in Contents.json"))? + pat.len();
    let end = text[at..]
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e' || c == 'E' || c == '+'))
        .map(|e| at + e)
        .unwrap_or(text.len());
    Ok(format!("{}{}{}", &text[..at], value, &text[end..]))
}

/// A photo background from a prepared layer HEIC and watch PNG.
pub fn build_photo(layer_heic: Vec<u8>, watch_png: Vec<u8>, d: Derived) -> Result<Vec<u8>> {
    let mut entries = read_zip(PHOTO_POSTER)?;
    let mut replaced = false;
    for e in entries.iter_mut() {
        if e.name.ends_with(HEIC_SUFFIX) {
            e.data = layer_heic.clone();
            replaced = true;
        } else if e.name.ends_with(CONTENTS_JSON) {
            let t = String::from_utf8(e.data.clone()).context("Contents.json is not UTF-8")?;
            e.data = set_json_number(&t, "clockAreaLuminance", d.clock_luminance)?.into_bytes();
        }
    }
    if !replaced {
        bail!("photo template has no portrait layer");
    }
    fresh_uuids(&mut entries);
    let poster = write_zip(&entries)?;
    let mut watch = Value::from_reader(Cursor::new(PHOTO_WATCH))?;
    let dict = watch.as_dictionary_mut().ok_or_else(|| anyhow!("watch template is not a dictionary"))?;
    dict.insert("isHighKey".into(), Value::Boolean(d.is_high_key));
    dict.insert("luminance".into(), Value::Real(d.luminance));
    dict.insert("backgroundImageData".into(), Value::Data(watch_png));
    outer(&watch, poster)
}

fn simple(ext: &str, user_info: Value, watch_png: Vec<u8>) -> Result<Vec<u8>> {
    let mut entries = read_zip(SIMPLE_POSTER)?;
    for e in entries.iter_mut() {
        if e.name == "manifest.plist" {
            // The captured manifest is an XML plist; swap the string as text
            // so the file is otherwise byte-identical.
            let from = format!("<string>{GRADIENT_EXT}</string>");
            let to = format!("<string>{ext}</string>");
            let next = replace_all(&e.data, from.as_bytes(), to.as_bytes());
            if ext != GRADIENT_EXT && next == e.data {
                bail!("template manifest does not name {GRADIENT_EXT}");
            }
            e.data = next;
        } else if e.name == USER_INFO {
            e.data = bplist(&user_info)?;
        }
    }
    fresh_uuids(&mut entries);
    let poster = write_zip(&entries)?;
    outer(&watch_dict(false, 0.2, watch_png, ext), poster)
}

/// RGBA in 0..=1.
pub type Rgba = [f64; 4];

/// Parse `#RRGGBB`, `#RRGGBBAA`, or `[r,g,b]` / `[r,g,b,a]` in 0..=1.
pub fn parse_color(v: &serde_json::Value) -> Result<Rgba> {
    match v {
        serde_json::Value::String(s) => {
            let h = s.trim_start_matches('#');
            if !(h.len() == 6 || h.len() == 8) || !h.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("color {s:?}: expected #RRGGBB or #RRGGBBAA");
            }
            let b = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f64 / 255.0;
            Ok([b(0), b(2), b(4), if h.len() == 8 { b(6) } else { 1.0 }])
        }
        serde_json::Value::Array(a) if a.len() == 3 || a.len() == 4 => {
            let mut c = [1.0; 4];
            for (i, x) in a.iter().enumerate() {
                let f = x.as_f64().ok_or_else(|| anyhow!("color component {x} is not a number"))?;
                if !(0.0..=1.0).contains(&f) {
                    bail!("color component {f} outside 0..1");
                }
                c[i] = f;
            }
            Ok(c)
        }
        other => bail!("color {other}: expected \"#RRGGBB\" or [r,g,b(,a)]"),
    }
}

/// The captured `custom` string: `r/g/b/a//r/g/b/a`, six decimals.
pub fn gradient_custom(colors: &[Rgba]) -> String {
    colors
        .iter()
        .map(|c| format!("{:.6}/{:.6}/{:.6}/{:.6}", c[0], c[1], c[2], c[3]))
        .collect::<Vec<_>>()
        .join("//")
}

fn gradient_png(colors: &[Rgba]) -> Result<Vec<u8>> {
    let (w, h) = WATCH_SIZE;
    let mut img = image::RgbaImage::new(w, h);
    let n = colors.len();
    for y in 0..h {
        let t = y as f64 / (h - 1) as f64 * (n - 1) as f64;
        let i = (t.floor() as usize).min(n - 2);
        let f = t - i as f64;
        let px = image::Rgba(std::array::from_fn(|k| {
            let v = colors[i][k] * (1.0 - f) + colors[i + 1][k] * f;
            (v * 255.0).round().clamp(0.0, 255.0) as u8
        }));
        for x in 0..w {
            img.put_pixel(x, y, px);
        }
    }
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

pub fn build_gradient(colors: &[Rgba]) -> Result<Vec<u8>> {
    if colors.len() != 2 {
        // Every capture has two; more is unverified.
        bail!("a gradient takes exactly two colors (as captured)");
    }
    let mut d = plist::Dictionary::new();
    d.insert("custom".into(), Value::String(gradient_custom(colors)));
    simple(GRADIENT_EXT, Value::Dictionary(d), gradient_png(colors)?)
}

pub fn build_dynamic(preset: &str) -> Result<Vec<u8>> {
    let (_, png) = DYNAMIC_PRESETS.iter().find(|(p, _)| *p == preset).ok_or_else(|| {
        anyhow!(
            "unknown dynamic preset {preset:?}; captured presets are {}",
            DYNAMIC_PRESETS.iter().map(|(p, _)| *p).collect::<Vec<_>>().join(", ")
        )
    })?;
    let mut d = plist::Dictionary::new();
    d.insert("identifier".into(), Value::String(preset.into()));
    simple(DYNAMIC_EXT, Value::Dictionary(d), png.to_vec())
}

/// Check a payload's structure without rustpush's poster parser, which
/// rejects iOS 26 photo posters. Returns (kind, watch preview bytes).
pub fn inspect(payload: &[u8]) -> Result<(String, Vec<u8>)> {
    let outer = read_zip(payload)?;
    let find = |n: &str| {
        outer.iter().find(|e| e.name.trim_start_matches('/') == n).ok_or_else(|| anyhow!("no {n}"))
    };
    let watch = Value::from_reader(Cursor::new(&find("transcriptBackground/watchBackground")?.data))
        .context("watchBackground is not a plist")?;
    let poster = read_zip(&find("transcriptBackground/poster")?.data).context("poster is not a zip")?;
    if !poster.iter().any(|e| e.name == "manifest.plist") {
        bail!("poster has no manifest.plist");
    }
    let w = watch.as_dictionary().ok_or_else(|| anyhow!("watchBackground is not a dictionary"))?;
    let ext = w.get("extensionIdentifier").and_then(Value::as_string).unwrap_or("");
    let kind = match ext {
        PHOTO_EXT => "photo",
        GRADIENT_EXT => "gradient",
        DYNAMIC_EXT => "dynamic",
        "com.apple.ContactsUI.MonogramPosterExtension" => "monogram",
        "com.apple.AvatarUI.AvatarPosterExtension" => "memoji",
        "" => "unknown",
        other => other,
    };
    let png = w.get("backgroundImageData").and_then(Value::as_data).unwrap_or_default().to_vec();
    Ok((kind.to_string(), png))
}

/// Run `prep_photo.py` on `src` with `python` (env `GROUPG_PYTHON`, else
/// `python`) and return (layer HEIC, watch PNG, derived values).
pub fn prep_photo(src: &str) -> Result<(Vec<u8>, Vec<u8>, Derived)> {
    if !std::path::Path::new(src).is_file() {
        bail!("{src} is not a file");
    }
    let dir = std::env::temp_dir().join(format!("ib-bg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("prep_photo.py");
    std::fs::write(&script, PREP_PHOTO_PY)?;
    let py = std::env::var("GROUPG_PYTHON").unwrap_or_else(|_| "python".into());
    let out = std::process::Command::new(&py)
        .arg("-I")
        .arg(&script)
        .arg(src)
        .arg(&dir)
        .output()
        .with_context(|| format!("running {py} (needs Pillow and pillow-heif)"))?;
    let result = (|| {
        if !out.status.success() {
            bail!("prep_photo.py failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let d: Derived = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("prep_photo.py output: {}", String::from_utf8_lossy(&out.stdout)))?;
        Ok((std::fs::read(dir.join("layer.heic"))?, std::fs::read(dir.join("watch.png"))?, d))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poster_of(payload: &[u8]) -> (Vec<Entry>, Value) {
        let outer = read_zip(payload).unwrap();
        assert_eq!(
            outer.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["transcriptBackground/watchBackground", "transcriptBackground/poster"]
        );
        assert!(outer.iter().all(|e| !e.stored));
        let watch = Value::from_reader(Cursor::new(&outer[0].data)).unwrap();
        (read_zip(&outer[1].data).unwrap(), watch)
    }

    /// Names with UUIDs normalised, in order, with methods.
    fn shape(entries: &[Entry]) -> Vec<(String, bool, bool)> {
        entries.iter().map(|e| (norm(&e.name), e.dir, e.stored)).collect()
    }

    fn norm(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = String::new();
        let mut i = 0;
        while i < b.len() {
            if i + 36 <= b.len() && uuid::Uuid::parse_str(&s[i..i + 36]).is_ok() {
                out.push_str("<UUID>");
                i += 36;
            } else {
                out.push(b[i] as char);
                i += 1;
            }
        }
        out
    }

    fn norm_bytes(d: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < d.len() {
            if i + 36 <= d.len() {
                if let Ok(s) = std::str::from_utf8(&d[i..i + 36]) {
                    if uuid::Uuid::parse_str(s).is_ok() && s.len() == 36 && s.contains('-') {
                        out.extend_from_slice(b"<UUID>");
                        i += 36;
                        continue;
                    }
                }
            }
            out.push(d[i]);
            i += 1;
        }
        out
    }

    fn tiny_png() -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image::RgbaImage::new(4, 4).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn photo_build_differs_only_in_image_derived_and_uuids() {
        let template = read_zip(PHOTO_POSTER).unwrap();
        let d = Derived { luminance: 0.42, clock_luminance: 0.33, is_high_key: false };
        let heic = b"FAKEHEIC".to_vec();
        let payload = build_photo(heic.clone(), tiny_png(), d).unwrap();
        let (poster, watch) = poster_of(&payload);
        assert_eq!(shape(&poster), shape(&template));
        let mut uuids = std::collections::HashSet::new();
        for (a, b) in poster.iter().zip(&template) {
            if a.name.ends_with(HEIC_SUFFIX) {
                assert_eq!(a.data, heic);
            } else if a.name.ends_with(CONTENTS_JSON) {
                let t = String::from_utf8(a.data.clone()).unwrap();
                assert!(t.contains("\"clockAreaLuminance\" : 0.33"));
                let back = set_json_number(&t, "clockAreaLuminance", 0.0).unwrap();
                let orig = set_json_number(std::str::from_utf8(&b.data).unwrap(), "clockAreaLuminance", 0.0).unwrap();
                assert_eq!(back, orig);
            } else {
                assert_eq!(norm_bytes(&a.data), norm_bytes(&b.data), "{}", a.name);
            }
            assert!(!a.name.contains("00000000-0000-4000-8000"), "placeholder left in {}", a.name);
            assert!(!a.data.windows(23).any(|w| w == b"00000000-0000-4000-8000"), "placeholder left in {}", a.name);
            for part in a.name.split('/') {
                if uuid::Uuid::parse_str(part).is_ok() {
                    uuids.insert(part.to_string());
                }
            }
        }
        assert_eq!(uuids.len(), 1, "one contents folder");
        let w = watch.as_dictionary().unwrap();
        assert_eq!(w.keys().collect::<Vec<_>>(), ["isHighKey", "luminance", "backgroundImageData", "extensionIdentifier"]);
        assert_eq!(w["luminance"].as_real(), Some(0.42));
        assert_eq!(w["extensionIdentifier"].as_string(), Some(PHOTO_EXT));
        assert_eq!(w["backgroundImageData"].as_data().unwrap(), tiny_png().as_slice());
        // The asset UUID in the folder name is the one ConfigurationModel and
        // userInfo name.
        let folder = uuids.into_iter().next().unwrap();
        let model = poster.iter().find(|e| e.name.ends_with("ConfigurationModel.plist")).unwrap();
        assert!(model.data.windows(36).any(|w| w == folder.as_bytes()));
        let ui = poster.iter().find(|e| e.name == USER_INFO).unwrap();
        assert!(ui.data.windows(36).any(|w| w == folder.as_bytes()));
    }

    #[test]
    fn two_builds_get_different_uuids() {
        let d = Derived { luminance: 0.1, clock_luminance: 0.1, is_high_key: false };
        let a = build_photo(vec![1], tiny_png(), d).unwrap();
        let b = build_photo(vec![1], tiny_png(), d).unwrap();
        let name = |p: &[u8]| poster_of(p).0.iter().find(|e| e.name.ends_with(HEIC_SUFFIX)).unwrap().name.clone();
        assert_ne!(name(&a), name(&b));
    }

    #[test]
    fn gradient_round_trip() {
        let colors = [parse_color(&serde_json::json!("#DCD2F2")).unwrap(), parse_color(&serde_json::json!([0.227451, 0.694118, 0.92549])).unwrap()];
        // The capture's own string.
        assert_eq!(gradient_custom(&colors), "0.862745/0.823529/0.949020/1.000000//0.227451/0.694118/0.925490/1.000000");
        let payload = build_gradient(&colors).unwrap();
        let (poster, watch) = poster_of(&payload);
        let template = read_zip(SIMPLE_POSTER).unwrap();
        assert_eq!(shape(&poster), shape(&template));
        for (a, b) in poster.iter().zip(&template) {
            if a.name != USER_INFO {
                assert_eq!(norm_bytes(&a.data), norm_bytes(&b.data), "{}", a.name);
            }
        }
        // Dynamic differs from gradient only in the manifest's extension and userInfo.
        let (dyn_poster, _) = poster_of(&build_dynamic("ocean_2").unwrap());
        for (a, b) in dyn_poster.iter().zip(&template) {
            if a.name == "manifest.plist" {
                let s = String::from_utf8(a.data.clone()).unwrap();
                assert_eq!(norm(&s), norm(&String::from_utf8(b.data.clone()).unwrap()).replace(GRADIENT_EXT, DYNAMIC_EXT));
            } else if a.name != USER_INFO {
                assert_eq!(norm_bytes(&a.data), norm_bytes(&b.data), "{}", a.name);
            }
        }
        let ui = Value::from_reader(Cursor::new(&poster.iter().find(|e| e.name == USER_INFO).unwrap().data)).unwrap();
        assert_eq!(ui.as_dictionary().unwrap()["custom"].as_string(), Some(gradient_custom(&colors).as_str()));
        let w = watch.as_dictionary().unwrap();
        assert_eq!(w["luminance"].as_real(), Some(0.2));
        assert_eq!(w["extensionIdentifier"].as_string(), Some(GRADIENT_EXT));
        let png = image::load_from_memory(w["backgroundImageData"].as_data().unwrap()).unwrap();
        assert_eq!((png.width(), png.height()), WATCH_SIZE);
        // rustpush parses what we built.
        let parsed = rustpush::posterkit::SimplifiedTranscriptPoster::parse_payload(&payload).unwrap();
        assert!(matches!(parsed.poster.r#type, rustpush::posterkit::PosterType::TranscriptGradient { .. }));
        assert!(build_gradient(&colors[..1]).is_err());
        assert!(parse_color(&serde_json::json!("blue")).is_err());
        assert!(parse_color(&serde_json::json!([2, 0, 0])).is_err());
    }

    #[test]
    fn dynamic_round_trip() {
        for (preset, png) in DYNAMIC_PRESETS {
            let payload = build_dynamic(preset).unwrap();
            let (poster, watch) = poster_of(&payload);
            let manifest = Value::from_reader(Cursor::new(&poster.iter().find(|e| e.name == "manifest.plist").unwrap().data)).unwrap();
            assert_eq!(manifest.as_dictionary().unwrap()["extensionIdentifier"].as_string(), Some(DYNAMIC_EXT));
            let ui = Value::from_reader(Cursor::new(&poster.iter().find(|e| e.name == USER_INFO).unwrap().data)).unwrap();
            assert_eq!(ui.as_dictionary().unwrap()["identifier"].as_string(), Some(*preset));
            assert_eq!(watch.as_dictionary().unwrap()["backgroundImageData"].as_data().unwrap(), *png);
            let parsed = rustpush::posterkit::SimplifiedTranscriptPoster::parse_payload(&payload).unwrap();
            let rustpush::posterkit::PosterType::TranscriptDynamic { data } = parsed.poster.r#type else { panic!() };
            assert_eq!(data.identifier, *preset);
        }
        assert!(build_dynamic("lava_9").is_err());
    }

    /// Runs the real photo pipeline when Python with pillow-heif is present.
    #[test]
    fn photo_from_image_end_to_end() {
        let src = std::env::temp_dir().join("ib-bg-src.png");
        let img = image::RgbImage::from_fn(600, 400, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        img.save(&src).unwrap();
        let (heic, png, d) = match prep_photo(src.to_str().unwrap()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("skipping: {e:#}");
                return;
            }
        };
        assert_eq!(&heic[4..8], b"ftyp");
        let w = image::load_from_memory(&png).unwrap();
        assert_eq!((w.width(), w.height()), WATCH_SIZE);
        assert!((0.0..=1.0).contains(&d.luminance) && (0.0..=1.0).contains(&d.clock_luminance));
        let payload = build_photo(heic.clone(), png, d).unwrap();
        let (poster, _) = poster_of(&payload);
        assert_eq!(poster.iter().find(|e| e.name.ends_with(HEIC_SUFFIX)).unwrap().data, heic);
        assert_eq!(inspect(&payload).unwrap().0, "photo");
    }

    #[test]
    fn inspect_kinds() {
        let d = Derived { luminance: 0.1, clock_luminance: 0.1, is_high_key: false };
        assert_eq!(inspect(&build_photo(vec![1], tiny_png(), d).unwrap()).unwrap().0, "photo");
        assert_eq!(inspect(&build_dynamic("clouds_2").unwrap()).unwrap().0, "dynamic");
        assert!(inspect(b"not a zip").is_err());
    }
}
