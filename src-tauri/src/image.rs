use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::coder::{arg_str, jail, rel_of};
use crate::coordinatore::provider_err;
use crate::view::b64_decode;

pub const MAX_JOB_IMAGES: usize = 8;
pub const DEFAULT_IMAGE_MODEL: &str = "nano-banana-2-fast";
pub const ASPECT_RATIOS: &[&str] = &[
    "1:1", "3:2", "2:3", "4:3", "3:4", "16:9", "9:16", "2:1", "1:2", "20:9", "9:20",
    "19.5:9", "9:19.5",
];

const MAX_BYTES: usize = 12 * 1024 * 1024;
const MAX_PROMPT: usize = 4000;

pub fn tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "create_image",
            "description": "Create a photo in the workspace (Nano Banana 2 Fast on NanoGPT). One image per call. Call several create_image in the same turn for different files — they run together. Do not wait one-by-one. Make only the images the page uses, at most 8 this job. User files first if they already exist. path is workspace-relative (images/hero.webp). prompt is the scene in English. aspect_ratio is required. resolution is 1k (default) or 2k for a wide hero. This model outputs 2k; 1k is sent as 2k.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative png, jpg, or webp. Example: images/hero.webp"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The scene. Subject, place, materials, time of day, light, lens. No letters, logos, or watermarks in the picture."
                    },
                    "aspect_ratio": {
                        "type": "string",
                        "enum": ["1:1", "3:2", "2:3", "4:3", "3:4", "16:9", "9:16", "2:1", "1:2", "20:9", "9:20", "19.5:9", "9:19.5"],
                        "description": "16:9 or 2:1 hero/banner. 4:3 or 3:2 section/card. 1:1 portrait/product. 3:4 or 9:16 tall. 20:9 full-bleed."
                    },
                    "resolution": {
                        "type": "string",
                        "enum": ["1k", "2k"],
                        "description": "1k default. 2k only for the main hero on a wide screen."
                    }
                },
                "required": ["path", "prompt", "aspect_ratio"]
            }
        }
    })
}

pub struct ImageOutcome {
    pub id: String,
    pub reply: String,
    pub shown: Option<String>,
}

struct Job {
    id: String,
    path: String,
    prompt: String,
    aspect_ratio: String,
    resolution: String,
    output_format: String,
}

#[derive(Clone)]
struct NanoImages {
    api_key: String,
    url: String,
    model: String,
    client: reqwest::Client,
}

impl NanoImages {
    fn from_env() -> Result<Self, String> {
        crate::coder::load_dotenv();
        let api_key = std::env::var("NANOGPT_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "Missing NANOGPT_API_KEY. create_image uses NanoGPT.".to_string()
            })?;
        let base = std::env::var("NANOGPT_BASE_URL")
            .unwrap_or_else(|_| "https://nano-gpt.com/api/v1".into());
        let url = std::env::var("NANOGPT_IMAGE_URL").unwrap_or_else(|_| {
            format!("{}/images", base.trim().trim_end_matches('/'))
        });
        let model = std::env::var("NANOGPT_IMAGE_MODEL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_IMAGE_MODEL.into());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(240))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            api_key,
            url,
            model,
            client,
        })
    }
}

pub fn normalize_aspect(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if ASPECT_RATIOS.iter().any(|a| *a == t) {
        return Ok(t.to_string());
    }
    Err(format!(
        "aspect_ratio must be one of: {}.",
        ASPECT_RATIOS.join(", ")
    ))
}

fn image_resolution_for_model<'a>(model: &str, asked: &'a str) -> &'a str {
    if model.contains("nano-banana-2-fast") && asked == "1k" {
        "2k"
    } else {
        asked
    }
}

pub fn normalize_resolution(raw: &str) -> Result<String, String> {
    let t = raw.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Ok("1k".into());
    }
    if t == "1k" || t == "2k" {
        return Ok(t);
    }
    Err("resolution must be 1k or 2k.".into())
}

fn output_format(path: &str) -> Result<String, String> {
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Ok("png".into()),
        "jpg" | "jpeg" => Ok("jpeg".into()),
        "webp" | "" => Ok("webp".into()),
        _ => Err("path must be a png, jpg, or webp.".into()),
    }
}

fn with_ext(path: &str, format: &str) -> String {
    let p = path.trim().trim_start_matches('/');
    if Path::new(p).extension().is_some() {
        return p.to_string();
    }
    let ext = if format == "jpeg" { "jpg" } else { format };
    format!("{p}.{ext}")
}

fn parse_job(id: String, args: &Value) -> Result<Job, String> {
    let path = arg_str(args, "path");
    if path.is_empty() {
        return Err("Missing path.".into());
    }
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if prompt.is_empty() {
        return Err("Missing prompt.".into());
    }
    if prompt.chars().count() > MAX_PROMPT {
        return Err("Prompt too long.".into());
    }
    let aspect_ratio = normalize_aspect(&arg_str(args, "aspect_ratio"))?;
    let resolution = normalize_resolution(&arg_str(args, "resolution"))?;
    let output_format = output_format(&path)?;
    let path = with_ext(&path, &output_format);
    Ok(Job {
        id,
        path,
        prompt,
        aspect_ratio,
        resolution,
        output_format,
    })
}

pub async fn create_many(
    root: &Path,
    calls: Vec<(String, Value)>,
    remaining: usize,
) -> Vec<ImageOutcome> {
    let mut planned: Vec<Result<Job, ImageOutcome>> = Vec::new();
    let mut allowed = remaining;
    for (id, args) in calls {
        if allowed == 0 {
            planned.push(Err(ImageOutcome {
                id,
                reply: format!(
                    "Error: at most {MAX_JOB_IMAGES} images this job. Use the files already created, or SVG for the rest."
                ),
                shown: None,
            }));
            continue;
        }
        match parse_job(id.clone(), &args) {
            Ok(job) => {
                allowed = allowed.saturating_sub(1);
                planned.push(Ok(job));
            }
            Err(e) => planned.push(Err(ImageOutcome {
                id,
                reply: format!("Error: {e}"),
                shown: None,
            })),
        }
    }

    let gw = match NanoImages::from_env() {
        Ok(g) => g,
        Err(e) => {
            return planned
                .into_iter()
                .map(|p| match p {
                    Ok(job) => ImageOutcome {
                        id: job.id,
                        reply: format!("Error: {e}"),
                        shown: None,
                    },
                    Err(out) => out,
                })
                .collect();
        }
    };

    let mut futs = Vec::new();
    let mut outcomes: Vec<Option<ImageOutcome>> = Vec::with_capacity(planned.len());
    for item in planned {
        match item {
            Err(out) => {
                outcomes.push(Some(out));
            }
            Ok(job) => {
                let idx = outcomes.len();
                outcomes.push(None);
                let root = root.to_path_buf();
                let gw = gw.clone();
                futs.push(async move {
                    let out = create_one(&gw, &root, job).await;
                    (idx, out)
                });
            }
        }
    }

    if !futs.is_empty() {
        for (idx, out) in futures::future::join_all(futs).await {
            outcomes[idx] = Some(out);
        }
    }

    outcomes
        .into_iter()
        .map(|o| o.expect("filled"))
        .collect()
}

async fn create_one(gw: &NanoImages, root: &Path, job: Job) -> ImageOutcome {
    match create_one_inner(gw, root, &job).await {
        Ok(shown) => ImageOutcome {
            id: job.id,
            reply: format!(
                "Created {shown} ({}, {}). Look at it. Use this path in the page.",
                job.aspect_ratio, job.resolution
            ),
            shown: Some(shown),
        },
        Err(e) => ImageOutcome {
            id: job.id,
            reply: format!("Error: {e}"),
            shown: None,
        },
    }
}

async fn create_one_inner(gw: &NanoImages, root: &Path, job: &Job) -> Result<String, String> {
    let path = jail(root, &job.path)?;
    let shown = rel_of(root, &path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{shown}: {e}"))?;
    }
    let resolution = image_resolution_for_model(&gw.model, &job.resolution);
    let body = json!({
        "model": gw.model,
        "prompt": job.prompt,
        "n": 1,
        "aspect_ratio": job.aspect_ratio,
        "resolution": resolution,
        "quality": "medium",
        "output_format": job.output_format
    });
    let response = gw
        .client
        .post(&gw.url)
        .bearer_auth(&gw.api_key)
        .header("x-api-key", &gw.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Image network: {e}"))?;
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .map_err(|e| format!("Image returned bad JSON: {e}"))?;
    if !status.is_success() {
        return Err(provider_err(&payload, status, "Image generation failed"));
    }
    if let Some(msg) = payload
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| payload.get("error").and_then(Value::as_str))
    {
        if !msg.is_empty() {
            return Err(msg.to_string());
        }
    }
    let bytes = bytes_from_payload(&gw.client, &payload).await?;
    if bytes.is_empty() {
        return Err("Empty image.".into());
    }
    if bytes.len() > MAX_BYTES {
        return Err("Image too large.".into());
    }
    std::fs::write(&path, &bytes).map_err(|e| format!("{shown}: {e}"))?;
    Ok(shown)
}

enum Blob {
    B64(String),
    Url(String),
}

fn collect_blobs(payload: &Value) -> Vec<Blob> {
    let mut out = Vec::new();
    let lists = [
        payload.get("data"),
        payload.get("images"),
        payload.pointer("/output/images"),
        payload.get("output"),
        payload.get("result"),
    ];
    for list in lists {
        let Some(v) = list else { continue };
        if let Some(arr) = v.as_array() {
            for item in arr {
                push_blob(item, &mut out);
            }
        } else {
            push_blob(v, &mut out);
        }
        if !out.is_empty() {
            break;
        }
    }
    if out.is_empty() {
        push_blob(payload, &mut out);
    }
    out
}

fn push_blob(item: &Value, out: &mut Vec<Blob>) {
    if let Some(s) = item.as_str() {
        out.push(classify(s));
        return;
    }
    let obj = match item.as_object() {
        Some(o) => o,
        None => return,
    };
    for key in ["b64_json", "b64", "base64", "image_base64"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                out.push(Blob::B64(s.to_string()));
                return;
            }
        }
    }
    for key in ["url", "image_url", "href"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            if !s.is_empty() {
                out.push(classify(s));
                return;
            }
        }
        if let Some(s) = obj
            .get(key)
            .and_then(|v| v.get("url"))
            .and_then(Value::as_str)
        {
            if !s.is_empty() {
                out.push(classify(s));
                return;
            }
        }
    }
    if let Some(s) = obj.get("image").and_then(Value::as_str) {
        if !s.is_empty() {
            out.push(classify(s));
        }
    }
}

fn classify(s: &str) -> Blob {
    let t = s.trim();
    if t.starts_with("data:") || looks_like_b64(t) {
        Blob::B64(t.to_string())
    } else {
        Blob::Url(t.to_string())
    }
}

fn looks_like_b64(s: &str) -> bool {
    let t = s.trim();
    t.len() > 80
        && !t.contains("://")
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '\n' | '\r'))
}

fn decode_b64_flex(raw: &str) -> Result<Vec<u8>, String> {
    let t = raw.trim();
    let t = if let Some((_, rest)) = t.split_once("base64,") {
        rest
    } else {
        t
    };
    let mut t: String = t.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    while t.len() % 4 != 0 {
        t.push('=');
    }
    b64_decode(&t)
}

async fn bytes_from_payload(client: &reqwest::Client, payload: &Value) -> Result<Vec<u8>, String> {
    let blobs = collect_blobs(payload);
    let Some(blob) = blobs.into_iter().next() else {
        return Err("NanoGPT returned no image.".into());
    };
    match blob {
        Blob::B64(s) => decode_b64_flex(&s),
        Blob::Url(u) => download(client, &u).await,
    }
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Image download: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Image download failed ({})", response.status()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Image download: {e}"))?;
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspect_accepts_model_ratios() {
        assert_eq!(normalize_aspect("16:9").unwrap(), "16:9");
        assert_eq!(normalize_aspect("19.5:9").unwrap(), "19.5:9");
        assert!(normalize_aspect("16x9").is_err());
        assert!(normalize_aspect("").is_err());
    }

    #[test]
    fn resolution_defaults_to_1k() {
        assert_eq!(normalize_resolution("").unwrap(), "1k");
        assert_eq!(normalize_resolution("2K").unwrap(), "2k");
        assert!(normalize_resolution("1024").is_err());
    }

    #[test]
    fn parse_job_fills_webp() {
        let job = parse_job(
            "1".into(),
            &json!({
                "path": "images/hero",
                "prompt": "A copper still in a brick distillery, morning light, 35mm",
                "aspect_ratio": "16:9"
            }),
        )
        .unwrap();
        assert_eq!(job.path, "images/hero.webp");
        assert_eq!(job.output_format, "webp");
        assert_eq!(job.resolution, "1k");
    }

    #[test]
    fn collect_b64_json() {
        let payload = json!({ "data": [{ "b64_json": "aGVsbG8=" }] });
        match &collect_blobs(&payload)[0] {
            Blob::B64(s) => assert_eq!(s, "aGVsbG8="),
            Blob::Url(_) => panic!("url"),
        }
    }

    #[test]
    fn collect_data_url() {
        let payload = json!({ "images": ["data:image/png;base64,aGVsbG8="] });
        match &collect_blobs(&payload)[0] {
            Blob::B64(s) => assert!(s.contains("aGVsbG8=")),
            Blob::Url(_) => panic!("url"),
        }
    }

    #[test]
    fn decode_pads() {
        let bytes = decode_b64_flex("aGVsbG8").unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn banana_fast_has_no_1k() {
        assert_eq!(
            image_resolution_for_model("nano-banana-2-fast", "1k"),
            "2k"
        );
        assert_eq!(
            image_resolution_for_model("nano-banana-2-fast", "2k"),
            "2k"
        );
        assert_eq!(
            image_resolution_for_model("xai/grok-imagine-image/v2.0/text-to-image", "1k"),
            "1k"
        );
    }

    #[test]
    fn tool_is_create_image() {
        let t = tool();
        assert_eq!(t["function"]["name"], json!("create_image"));
        let req = t["function"]["parameters"]["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "prompt"));
        assert!(req.iter().any(|v| v == "aspect_ratio"));
    }
}
