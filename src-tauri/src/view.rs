use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use crate::coder::{jail, rel_of};

const CHROME_CANDIDATES: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
];

const DEFAULT_WIDTHS: &[u32] = &[390, 1280];
const MIN_SHOT: u64 = 8_000;
const WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct PageShot {
    pub width: u32,
    #[allow(dead_code)]
    pub shown: String,
    pub mime: String,
    pub b64: String,
}

pub fn chrome_bin() -> Result<PathBuf, String> {
    if let Some(raw) = crate::coordinatore::env_puck("PUCK_CHROME") {
        let p = PathBuf::from(raw.trim());
        if p.is_file() {
            return Ok(p);
        }
    }
    for c in CHROME_CANDIDATES {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err("Chrome is not installed. The Coder needs Google Chrome (or Chromium / Edge / Brave) to open the page.".into())
}

fn file_url(path: &Path) -> String {
    let s = path.to_string_lossy().replace(' ', "%20");
    format!("file://{s}")
}

fn height_for(width: u32) -> u32 {
    match width {
        w if w <= 500 => 1800,
        w if w <= 900 => 2000,
        _ => 2400,
    }
}

fn shot_dir(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join(".puck-review");
    fs::create_dir_all(&dir).map_err(|e| format!("Screenshot folder: {e}"))?;
    Ok(dir)
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

fn wait_for_file(path: &Path, child: &mut std::process::Child) -> Result<Vec<u8>, String> {
    let start = Instant::now();
    let mut last = 0u64;
    let mut stable = 0u8;
    loop {
        if let Ok(meta) = fs::metadata(path) {
            let n = meta.len();
            if n >= MIN_SHOT {
                if n == last {
                    stable += 1;
                    if stable >= 3 {
                        kill_group(child);
                        let _ = child.wait();
                        return fs::read(path).map_err(|e| format!("Read screenshot: {e}"));
                    }
                } else {
                    stable = 0;
                    last = n;
                }
            }
        }
        if start.elapsed() > WAIT {
            kill_group(child);
            let _ = child.wait();
            return Err("Timed out waiting for the page screenshot.".into());
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                if let Ok(bytes) = fs::read(path) {
                    if bytes.len() as u64 >= MIN_SHOT {
                        return Ok(bytes);
                    }
                }
                return Err("Chrome closed before a screenshot was written.".into());
            }
            Ok(None) => thread::sleep(Duration::from_millis(120)),
            Err(e) => {
                kill_group(child);
                return Err(format!("Chrome: {e}"));
            }
        }
    }
}

fn maybe_jpeg(png: &[u8], dest: &Path) -> (String, Vec<u8>) {
    if png.len() <= 1_200_000 {
        return ("image/png".into(), png.to_vec());
    }
    let jpg = dest.with_extension("jpg");
    let ok = Command::new("sips")
        .args([
            "-s",
            "format",
            "jpeg",
            "-s",
            "formatOptions",
            "70",
            dest.to_str().unwrap_or(""),
            "--out",
            jpg.to_str().unwrap_or(""),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        if let Ok(bytes) = fs::read(&jpg) {
            if !bytes.is_empty() && bytes.len() < png.len() {
                return ("image/jpeg".into(), bytes);
            }
        }
    }
    ("image/png".into(), png.to_vec())
}

fn capture_one(
    chrome: &Path,
    html: &Path,
    out: &Path,
    width: u32,
) -> Result<Vec<u8>, String> {
    let profile = std::env::temp_dir().join(format!(
        "puck-chrome-{}-{}-{}",
        std::process::id(),
        width,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::create_dir_all(&profile);
    let _ = fs::remove_file(out);
    let height = height_for(width);
    let mut cmd = Command::new(chrome);
    cmd.args([
        "--headless=new",
        "--disable-gpu",
        "--hide-scrollbars",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-sync",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-component-update",
        "--metrics-recording-only",
        "--mute-audio",
        "--no-proxy-server",
        "--deny-permission-prompts",
        "--allow-file-access-from-files",
        "--disable-features=Translate,MediaRouter,OptimizationHints",
    ])
    .arg(format!("--user-data-dir={}", profile.display()))
    .arg(format!("--window-size={width},{height}"))
    .arg(format!("--screenshot={}", out.display()))
    .arg(file_url(html))
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("Could not start Chrome: {e}"))?;
    let bytes = wait_for_file(out, &mut child);
    let _ = fs::remove_dir_all(&profile);
    bytes
}

pub(crate) fn b64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        if i + 1 < data.len() {
            out.push(A[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < data.len() {
            out.push(A[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

pub(crate) fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Result<u8, String> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => 0,
            _ => return Err("Bad base64.".into()),
        })
    }
    let s: String = s.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if s.is_empty() || s.len() % 4 != 0 {
        return Err("Bad base64.".into());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut i = 0;
    while i + 3 < bytes.len() {
        let n = ((val(bytes[i])? as u32) << 18)
            | ((val(bytes[i + 1])? as u32) << 12)
            | ((val(bytes[i + 2])? as u32) << 6)
            | (val(bytes[i + 3])? as u32);
        out.push((n >> 16) as u8);
        if bytes[i + 2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if bytes[i + 3] != b'=' {
            out.push(n as u8);
        }
        i += 4;
    }
    Ok(out)
}

const RASTER: &[&str] = &["png", "jpg", "jpeg", "webp", "gif"];
const SKIP_WALK: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "dist",
    ".DS_Store",
    ".puck-review",
];
const MAX_RAW: u64 = 8_000_000;
const MAX_ATTACH: usize = 8;

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_raster(path: &Path) -> bool {
    RASTER.contains(&ext_of(path).as_str())
}

pub fn is_svg(path: &Path) -> bool {
    ext_of(path) == "svg"
}

fn mime_of(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

fn load_raster(path: &Path) -> Result<(String, Vec<u8>), String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_RAW {
        return Err("Image too large.".into());
    }
    let bytes = fs::read(path).map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("Empty image.".into());
    }
    let ext = ext_of(path);
    if ext == "png" && bytes.len() > 1_200_000 {
        return Ok(maybe_jpeg(&bytes, path));
    }
    Ok((mime_of(&ext).to_string(), bytes))
}

pub fn view_image(root: &Path, rel: &str) -> Result<(String, PageShot), String> {
    let path = jail(root, rel)?;
    if !path.is_file() {
        return Err(format!("{} is not a file.", rel_of(root, &path)));
    }
    if is_svg(&path) {
        return Err("SVG is text. Call read_file on this path.".into());
    }
    if !is_raster(&path) {
        return Err("view_image is for png, jpg, webp, or gif.".into());
    }
    let shown = rel_of(root, &path);
    let (mime, bytes) = load_raster(&path)?;
    Ok((
        format!("Opened {shown} ({} bytes). Look at the image.", bytes.len()),
        PageShot {
            width: 0,
            shown,
            mime,
            b64: b64(&bytes),
        },
    ))
}

pub fn list_image_paths(root: &Path) -> Result<Vec<String>, String> {
    let start = jail(root, ".")?;
    let mut hits = Vec::new();
    fn walk(root: &Path, dir: &Path, hits: &mut Vec<String>) -> Result<(), String> {
        if hits.len() >= 80 {
            return Ok(());
        }
        let rd = fs::read_dir(dir).map_err(|e| e.to_string())?;
        for ent in rd {
            if hits.len() >= 80 {
                break;
            }
            let ent = ent.map_err(|e| e.to_string())?;
            let name = ent.file_name().to_string_lossy().to_string();
            if SKIP_WALK.iter().any(|s| name == *s) {
                continue;
            }
            let path = ent.path();
            if path.is_dir() {
                walk(root, &path, hits)?;
                continue;
            }
            if is_raster(&path) || is_svg(&path) {
                hits.push(rel_of(root, &path));
            }
        }
        Ok(())
    }
    walk(root, &start, &mut hits)?;
    hits.sort();
    Ok(hits)
}

pub fn paths_in_text(text: &str) -> Vec<String> {
    let re = Regex::new(r"(?i)([\w./\\-]+\.(?:png|jpe?g|webp|gif|svg))").ok();
    let Some(re) = re else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for cap in re.captures_iter(text) {
        let raw = cap[1].replace('\\', "/");
        let rel = raw.trim_start_matches("./");
        if rel.is_empty() || rel.contains("..") {
            continue;
        }
        if !out.iter().any(|s| s == rel) {
            out.push(rel.to_string());
        }
        if out.len() >= MAX_ATTACH {
            break;
        }
    }
    out
}

pub fn load_named_images(root: &Path, text: &str) -> Vec<PageShot> {
    let mut out = Vec::new();
    for rel in paths_in_text(text) {
        if out.len() >= MAX_ATTACH {
            break;
        }
        if let Ok((_, shot)) = view_image(root, &rel) {
            out.push(shot);
        }
    }
    out
}

fn is_html(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("html") | Some("htm")
    )
}

pub fn view_page(
    root: &Path,
    rel: &str,
    width: Option<u32>,
) -> Result<(String, Vec<PageShot>), String> {
    let html = jail(root, rel)?;
    if !html.is_file() {
        return Err(format!("{} is not a file.", rel_of(root, &html)));
    }
    if !is_html(&html) {
        return Err("view_page opens an HTML page. Pass the .html file.".into());
    }
    let chrome = chrome_bin()?;
    let dir = shot_dir(root)?;
    let widths: Vec<u32> = match width {
        Some(w) if (320..=1920).contains(&w) => vec![w],
        Some(_) => return Err("width must be between 320 and 1920.".into()),
        None => DEFAULT_WIDTHS.to_vec(),
    };
    let shown = rel_of(root, &html);
    let mut shots = Vec::new();
    let mut lines = vec![format!("Opened {shown}. Look at the screenshots. Then one complete report.")];
    for w in widths {
        let out = dir.join(format!("{}-{w}.png", stem(&html)));
        let png = capture_one(&chrome, &html, &out, w)?;
        fs::write(&out, &png).map_err(|e| format!("Save screenshot: {e}"))?;
        let (mime, bytes) = maybe_jpeg(&png, &out);
        lines.push(format!("  {w}px  {}  {} bytes", rel_of(root, &out), bytes.len()));
        shots.push(PageShot {
            width: w,
            shown: shown.clone(),
            mime,
            b64: b64(&bytes),
        });
    }
    Ok((lines.join("\n"), shots))
}

pub fn seen_reply(text: String, sees: bool) -> String {
    if sees {
        text
    } else {
        format!(
            "{text} This model cannot see pixels. Use the path in HTML src. Do not call view_image or view_page again on the same file to look at pixels."
        )
    }
}

pub fn images_followup(shots: &[PageShot], text: &str) -> Value {
    if shots.is_empty() {
        return json!({ "role": "user", "content": text });
    }
    let mut parts = vec![json!({ "type": "text", "text": text })];
    for shot in shots {
        parts.push(json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:{};base64,{}", shot.mime, shot.b64)
            }
        }));
    }
    json!({ "role": "user", "content": parts })
}

pub fn image_followup(shots: &[PageShot]) -> Value {
    images_followup(
        shots,
        "These are screenshots of the page (phone, then desktop unless you asked one width). Look at the whole page. Then write one complete report: every hole, or ok. Not one item.",
    )
}

pub fn thin_report(spoken: &str) -> bool {
    let t = spoken.to_ascii_lowercase();
    if t.contains("**ok**") || t.lines().next().is_some_and(|l| l.trim() == "ok") {
        return false;
    }
    let markers = spoken
        .matches("**file**")
        .count()
        .max(spoken.matches("- **file**").count())
        .max(
            spoken
                .lines()
                .filter(|l| {
                    let s = l.trim().to_ascii_lowercase();
                    s.starts_with("file —") || s.starts_with("file:") || s.starts_with("### finding")
                })
                .count(),
        );
    markers <= 1 && spoken.chars().count() < 900
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_encodes_spaces() {
        assert_eq!(
            file_url(Path::new("/tmp/a page.html")),
            "file:///tmp/a%20page.html"
        );
    }

    #[test]
    fn ok_is_not_thin() {
        assert!(!thin_report("**ok**\nThe page matches the brief."));
        assert!(thin_report(
            "### Finding\n- **file** — a.html\n- **where** — footer"
        ));
    }

    #[test]
    fn b64_roundtrip_len() {
        let s = b64(b"hi");
        assert_eq!(s, "aGk=");
        assert_eq!(b64_decode(&s).unwrap(), b"hi");
        assert_eq!(b64_decode("aGVsbG8=").unwrap(), b"hello");
    }

    #[test]
    fn paths_in_text_finds_images() {
        let hits = paths_in_text("Use logo.png and photos/mood.jpg. Skip ../secret.png");
        assert!(hits.contains(&"logo.png".into()), "{hits:?}");
        assert!(hits.contains(&"photos/mood.jpg".into()), "{hits:?}");
        assert!(!hits.iter().any(|s| s.contains("..")));
    }

    #[test]
    fn view_image_reads_png() {
        let root = std::env::temp_dir().join(format!(
            "puck-img-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xFE,
            0xD4, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        fs::write(root.join("logo.png"), png).unwrap();
        fs::write(root.join("note.svg"), "<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
        let (text, shot) = view_image(&root, "logo.png").unwrap();
        assert!(text.contains("logo.png"));
        assert!(seen_reply(text.clone(), true).contains("Look at the image"));
        assert!(seen_reply(text.clone(), false).contains("cannot see pixels"));
        assert_eq!(shot.mime, "image/png");
        assert!(!shot.b64.is_empty());
        assert!(view_image(&root, "note.svg").unwrap_err().contains("SVG"));
        assert!(view_image(&root, "../secret.png").is_err());
        let listed = list_image_paths(&root).unwrap();
        assert!(listed.contains(&"logo.png".into()));
        assert!(listed.contains(&"note.svg".into()));
        let _ = fs::remove_dir_all(&root);
    }
}
