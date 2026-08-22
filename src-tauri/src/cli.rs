// Dev-only channel: drive the window from a terminal the same way a
// finger would. File-based, no extra crate. Drop a command into
// `cli-order.txt` (project root). This watcher picks it up and emits
// `puck-cli`. The frontend runs the same functions as the buttons
// (send, answer, skip, clear, folder, attach, detach). This file never
// talks to the model or the backend order logic. Debug builds only.
//
// Plain text is still an order (same as typing and pressing Enter).
// JSON is `{ "op": "say"|"answer"|"skip"|"clear"|"folder"|"attach"|"detach", ... }`.
// The window writes `cli-order.out.json` as an ack. `say` then waits
// on `context/run.jsonl` (`order_end`) like before.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

const MAX_ATTACH_BYTES: u64 = 12 * 1024 * 1024;

fn root_dir() -> PathBuf {
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    if from_crate.is_dir() {
        return from_crate;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn in_path() -> PathBuf {
    match crate::coordinatore::env_puck("PUCK_CLI_IN") {
        Some(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => root_dir().join("cli-order.txt"),
    }
}

fn out_path() -> PathBuf {
    match crate::coordinatore::env_puck("PUCK_CLI_OUT") {
        Some(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => root_dir().join("cli-order.out.json"),
    }
}

fn parse_cmd(raw: &str) -> Option<Value> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('{') {
        serde_json::from_str::<Value>(text)
            .ok()
            .filter(|v| v.get("op").and_then(Value::as_str).is_some())
    } else {
        Some(json!({ "op": "say", "text": text }))
    }
}

/// Spawn the watcher. Call once from `setup`. No-op in release builds.
pub fn spawn_watcher(app: AppHandle) {
    if !cfg!(debug_assertions) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let input = in_path();
        crate::context::run_log(
            "cli_watch",
            json!({
                "in": input.display().to_string(),
                "out": out_path().display().to_string(),
            }),
        );
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let Ok(raw) = std::fs::read_to_string(&input) else {
                continue;
            };
            let Some(cmd) = parse_cmd(&raw) else {
                continue;
            };
            let _ = std::fs::remove_file(&input);
            let _ = app.emit("puck-cli", cmd);
        }
    });
}

#[tauri::command]
pub fn cli_ack(payload: Value) -> Result<(), String> {
    if !cfg!(debug_assertions) {
        return Err("debug only".into());
    }
    let raw = serde_json::to_vec_pretty(&payload).map_err(|e| e.to_string())?;
    let path = out_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, raw).map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct CliFile {
    name: String,
    data: String,
    image: bool,
}

#[tauri::command]
pub fn cli_load_file(path: String) -> Result<CliFile, String> {
    if !cfg!(debug_assertions) {
        return Err("debug only".into());
    }
    let path = PathBuf::from(path.trim());
    if !path.is_file() {
        return Err("That is not a file.".into());
    }
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_ATTACH_BYTES {
        return Err("File is too large.".into());
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Err("File is empty.".into());
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    Ok(CliFile {
        image: is_image(&path),
        name,
        data: crate::view::b64(&bytes),
    })
}

fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg")
    )
}

#[cfg(test)]
mod tests {
    use super::parse_cmd;
    use serde_json::json;

    #[test]
    fn plain_text_is_say() {
        assert_eq!(
            parse_cmd("  ciao  "),
            Some(json!({ "op": "say", "text": "ciao" }))
        );
        assert_eq!(parse_cmd("   "), None);
    }

    #[test]
    fn json_needs_op() {
        assert_eq!(
            parse_cmd(r#"{ "op": "skip" }"#),
            Some(json!({ "op": "skip" }))
        );
        assert_eq!(parse_cmd(r#"{ "text": "ciao" }"#), None);
        assert_eq!(parse_cmd("{nope"), None);
    }
}
