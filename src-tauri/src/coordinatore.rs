use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Datelike;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::context::{
    absorb_thread, compact_comune_if_needed, compact_project_if_needed, fill_threads_from_disk,
    inject_summary, pack_thread, persist_after_ask, save_turns, Memory,
};

pub(crate) fn today_line() -> String {
    let n = chrono::Local::now().date_naive();
    let weekday = match n.weekday() {
        chrono::Weekday::Mon => "Monday",
        chrono::Weekday::Tue => "Tuesday",
        chrono::Weekday::Wed => "Wednesday",
        chrono::Weekday::Thu => "Thursday",
        chrono::Weekday::Fri => "Friday",
        chrono::Weekday::Sat => "Saturday",
        chrono::Weekday::Sun => "Sunday",
    };
    let month = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ][n.month0() as usize];
    format!(
        "Today is {weekday}, {} {month} {}.",
        n.day(),
        n.year()
    )
}

pub(crate) fn role_system(standing: &str, summary: &str) -> String {
    inject_summary(&standing_prefix(standing), summary)
}

pub(crate) fn standing_prefix(standing: &str) -> String {
    format!("{}\n\n{}", standing.trim_end(), today_line())
}

fn cache_ctrl() -> Value {
    json!({ "type": "ephemeral", "ttl": "1h" })
}

/// Standing orders + today's date in a cached prefix. Thread summary stays
/// outside so it cannot bust the cache on every compact.
pub(crate) fn system_turns(standing: &str, summary: &str) -> Vec<Value> {
    let mut out = vec![json!({
        "role": "system",
        "content": [{
            "type": "text",
            "text": standing_prefix(standing),
            "cache_control": cache_ctrl()
        }]
    })];
    let summary = summary.trim();
    if !summary.is_empty() {
        out.push(json!({
            "role": "system",
            "content": format!("## This thread\n\n{summary}")
        }));
    }
    out
}

pub(crate) fn mark_tools_cached(tools: &[Value]) -> Vec<Value> {
    let mut out = tools.to_vec();
    if let Some(last) = out.last_mut() {
        last["cache_control"] = cache_ctrl();
    }
    out
}

pub(crate) fn with_prompt_cache(body: &mut Value, tools: Option<&[Value]>, cache_key: &str) {
    body["prompt_caching"] = json!({ "ttl": "1h" });
    if !cache_key.is_empty() {
        body["prompt_cache_key"] = json!(cache_key);
    }
    if let Some(tools) = tools {
        body["tools"] = json!(mark_tools_cached(tools));
    }
}

fn current_awake(awake: &[String], woke: &[String]) -> String {
    let mut names = if awake.is_empty() {
        vec!["coordinatore".to_string()]
    } else {
        awake.to_vec()
    };
    for w in woke {
        if !names.iter().any(|n| n == w) {
            names.push(w.clone());
        }
    }
    names.join(", ")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct WorkPiece {
    pub role: String,
    pub brief: String,
    pub text: String,
    pub from: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AskReply {
    pub text: String,
    pub woke: Vec<String>,
    pub work: Vec<WorkPiece>,
}

pub(crate) struct RoleCard {
    tool: Option<String>,
    description: Option<String>,
    body: String,
}

#[derive(Clone)]
pub(crate) struct Llm {
    pub(crate) api_key: String,
    pub(crate) url: String,
    pub(crate) model: String,
    pub(crate) client: reqwest::Client,
}

impl Llm {
    pub(crate) fn is_openrouter(&self) -> bool {
        self.url.contains("openrouter.ai")
    }

    pub(crate) fn is_cerebras(&self) -> bool {
        self.url.contains("api.cerebras.ai")
    }

    pub(crate) fn is_google(&self) -> bool {
        self.url.contains("generativelanguage.googleapis.com")
            || self.url.contains("aiplatform.googleapis.com")
    }

    /// Official DeepSeek API (OpenAI-compatible). Distinct from DeepSeek
    /// models routed through NanoGPT, which share the model name check.
    pub(crate) fn is_deepseek_api(&self) -> bool {
        self.url.contains("api.deepseek.com")
    }

    /// Gemini's OpenAI-compat endpoint and the DeepSeek API reject
    /// NanoGPT extras; DeepSeek also has no `reasoning` object in the
    /// OpenAI format (thinking + reasoning_effort are enough).
    pub(crate) fn prepare_body(&self, body: &mut Value) {
        if !(self.is_google() || self.is_deepseek_api() || self.is_cerebras()) {
            return;
        }
        if let Some(obj) = body.as_object_mut() {
            obj.remove("reasoning");
            obj.remove("prompt_caching");
            obj.remove("prompt_cache_key");
        }
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                if let Some(obj) = tool.as_object_mut() {
                    obj.remove("cache_control");
                }
            }
        }
        if let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for msg in msgs {
                if let Some(parts) = msg.get_mut("content").and_then(Value::as_array_mut) {
                    for part in parts {
                        if let Some(obj) = part.as_object_mut() {
                            obj.remove("cache_control");
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn is_gemini(&self) -> bool {
        self.model.to_ascii_lowercase().contains("gemini")
    }

    pub(crate) fn is_grok(&self) -> bool {
        self.model.to_ascii_lowercase().contains("grok")
    }

    pub(crate) fn is_deepseek(&self) -> bool {
        self.model.to_ascii_lowercase().contains("deepseek")
    }

    pub(crate) fn is_glm(&self) -> bool {
        self.model.to_ascii_lowercase().contains("glm")
    }

    pub(crate) fn is_muse(&self) -> bool {
        self.model.to_ascii_lowercase().contains("muse")
    }

    pub(crate) fn is_openai(&self) -> bool {
        let m = self.model.to_ascii_lowercase();
        m.contains("gpt-") || m.starts_with("openai/")
    }

    pub(crate) fn sees_images(&self) -> bool {
        // Official DeepSeek API: the vision-exp model takes image_url parts.
        if self.is_deepseek_api() {
            return true;
        }
        // GLM and DeepSeek on NanoGPT reject image_url (HTTP 400).
        if self.is_glm() || self.is_deepseek() {
            return false;
        }
        // Cerebras e' testo solo.
        if self.is_cerebras() {
            return false;
        }
        self.is_gemini() || self.is_grok() || self.is_muse() || {
            let m = self.model.to_ascii_lowercase();
            m.contains("gpt-") || m.contains("claude")
        }
    }

    pub(crate) fn max_tokens(&self) -> u32 {
        if self.is_gemini() || self.is_muse() {
            65_536
        } else if self.is_glm() {
            131_072
        } else if self.is_cerebras() {
            32_768
        } else {
            384_000
        }
    }

    pub(crate) fn bind(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.uses_native_gemini() {
            return req.header("x-goog-api-key", &self.api_key);
        }
        let req = req.bearer_auth(&self.api_key);
        if self.is_openrouter() {
            req.header("HTTP-Referer", "https://askpuck.app")
                .header("X-Title", "Puck")
        } else {
            req
        }
    }

    pub(crate) fn thinking_extra(&self) -> Value {
        self.thinking_extra_for("coder")
    }

    pub(crate) fn thinking_extra_for(&self, role: &str) -> Value {
        self.extra_for_effort(role_effort(role))
    }

    fn clamp_effort(&self, want: &str) -> String {
        let w = want.to_ascii_lowercase();
        if self.is_gemini() {
            let lite = self.model.to_ascii_lowercase();
            let allows_minimal = lite.contains("lite")
                || lite.contains("3.5")
                || lite.contains("3.6");
            return match w.as_str() {
                "none" | "minimal" if allows_minimal => "minimal".into(),
                "none" | "low" => "low".into(),
                "medium" => "medium".into(),
                "high" => "high".into(),
                _ => "high".into(),
            };
        }
        if self.is_deepseek() {
            return match w.as_str() {
                "none" | "low" => "low".into(),
                "max" => "max".into(),
                _ => "high".into(),
            };
        }
        if self.is_grok() {
            return match w.as_str() {
                "none" | "low" | "medium" | "high" | "xhigh" => w,
                _ => "xhigh".into(),
            };
        }
        match w.as_str() {
            "none" | "low" | "medium" | "high" | "xhigh" | "max" => w,
            _ => "medium".into(),
        }
    }

    fn extra_for_effort(&self, want: &str) -> Value {
        // Official DeepSeek API: thinking on by default, effort low/high/max.
        // No `reasoning` object in the OpenAI format — prepare_body strips it.
        if self.is_deepseek_api() {
            let effort = self.clamp_effort(want);
            return json!({
                "reasoning_effort": effort,
                "thinking": { "type": "enabled" },
            });
        }
        // Cerebras: solo reasoning_effort; l'oggetto `reasoning` e' rifiutato.
        if self.is_cerebras() {
            let effort = self.clamp_effort(want);
            return json!({ "reasoning_effort": effort });
        }
        let effort = self.clamp_effort(want);
        if self.is_google() {
            let level = match effort.as_str() {
                "none" | "low" => "LOW",
                "medium" => "MEDIUM",
                _ => "HIGH",
            };
            json!({
                "extra_body": {
                    "google": {
                        "thinking_config": {
                            "include_thoughts": true,
                            "thinking_level": level
                        }
                    }
                }
            })
        } else if self.is_gemini() {
            json!({
                "reasoning_effort": effort,
                "reasoning": { "effort": effort, "enabled": true }
            })
        } else if self.is_grok() {
            json!({
                "reasoning_effort": effort,
                "reasoning": { "effort": effort, "enabled": true }
            })
        } else if self.is_deepseek() {
            json!({
                "reasoning_effort": effort,
                "thinking": { "type": "enabled" },
                "reasoning": { "effort": effort, "enabled": true }
            })
        } else if self.is_glm() {
            json!({
                "reasoning_effort": effort,
                "thinking": { "type": "enabled" },
                "reasoning": { "effort": effort, "enabled": true }
            })
        } else if self.is_openrouter() && !self.is_openai() {
            json!({ "reasoning": { "effort": effort, "enabled": true } })
        } else if self.is_openai() {
            json!({
                "reasoning_effort": effort,
                "reasoning": { "effort": effort, "enabled": true }
            })
        } else {
            json!({
                "reasoning_effort": effort,
                "thinking": { "type": "enabled" },
                "reasoning": { "effort": effort }
            })
        }
    }
}

pub(crate) fn role_effort(role: &str) -> &'static str {
    match role {
        "coordinatore" => "medium",
        "coder" => "high",
        _ => "medium",
    }
}

pub(crate) const DEFAULT_CHAT_MODEL: &str = "openai/gpt-5.6-luna";
pub(crate) const DEFAULT_GOOGLE_MODEL: &str = "gemini-3.7-flash";
pub(crate) const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-v4-flash-vision-exp";

pub(crate) fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `PUCK_*` with `NYNI_*` fallback so an old `.env` still works.
pub(crate) fn env_puck(key: &str) -> Option<String> {
    env_nonempty(key).or_else(|| env_nonempty(&key.replacen("PUCK_", "NYNI_", 1)))
}

fn pick_env(keys: &[&str], default: &str) -> String {
    for k in keys {
        if let Some(v) = env_nonempty(k) {
            return v;
        }
    }
    default.to_string()
}

const LLM_CONNECT_SECS: u64 = 30;
const LLM_TIMEOUT_SECS: u64 = 1_800;

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(LLM_CONNECT_SECS))
        .timeout(Duration::from_secs(LLM_TIMEOUT_SECS))
        .build()
        .map_err(|e| e.to_string())
}

pub(crate) fn llm_http_err(e: reqwest::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        parts.push(s.to_string());
        src = s.source();
    }
    let joined = parts.join(": ");
    if e.is_timeout() || is_timeout_msg(&joined) {
        format!("Timed out waiting for the model ({joined})")
    } else {
        joined
    }
}

pub(crate) fn is_timeout_msg(msg: &str) -> bool {
    let t = msg.to_ascii_lowercase();
    t.contains("timed out") || t.contains("timeout")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatBackend {
    Nano,
    OpenRouter,
    Google,
    DeepSeek,
    Cerebras,
}

fn chat_backend_from(raw: &str) -> ChatBackend {
    match raw.trim().to_ascii_lowercase().as_str() {
        "openrouter" => ChatBackend::OpenRouter,
        "google" | "gemini" => ChatBackend::Google,
        "deepseek" => ChatBackend::DeepSeek,
        "cerebras" => ChatBackend::Cerebras,
        _ => ChatBackend::Nano,
    }
}

fn chat_backend() -> ChatBackend {
    chat_backend_from(&env_nonempty("PUCK_CHAT").unwrap_or_default())
}

pub(crate) fn llm_from_env(
    nano_keys: &[&str],
    openrouter_keys: &[&str],
    google_keys: &[&str],
    deepseek_keys: &[&str],
    cerebras_keys: &[&str],
) -> Result<Llm, String> {
    crate::coder::load_dotenv();
    match chat_backend() {
        ChatBackend::DeepSeek => {
            if let Some(llm) = llm_deepseek(deepseek_keys)? {
                return Ok(llm);
            }
        }
        ChatBackend::Cerebras => {
            if let Some(llm) = llm_cerebras(cerebras_keys)? {
                return Ok(llm);
            }
        }
        ChatBackend::Google => {
            if let Some(llm) = llm_google(google_keys)? {
                return Ok(llm);
            }
        }
        ChatBackend::OpenRouter => {
            if let Some(llm) = llm_openrouter(openrouter_keys)? {
                return Ok(llm);
            }
        }
        ChatBackend::Nano => {
            if let Some(llm) = llm_nanogpt(nano_keys)? {
                return Ok(llm);
            }
        }
    }
    if let Some(llm) = llm_deepseek(deepseek_keys)? {
        return Ok(llm);
    }
    if let Some(llm) = llm_google(google_keys)? {
        return Ok(llm);
    }
    if let Some(llm) = llm_openrouter(openrouter_keys)? {
        return Ok(llm);
    }
    if let Some(llm) = llm_nanogpt(nano_keys)? {
        return Ok(llm);
    }
    Err(
        "Missing DEEPSEEK_API_KEY, NANOGPT_API_KEY, OPENROUTER_API_KEY, or GOOGLE_API_KEY. Copy .env.example to .env."
            .into(),
    )
}

fn llm_nanogpt(keys: &[&str]) -> Result<Option<Llm>, String> {
    let Some(api_key) = env_nonempty("NANOGPT_API_KEY") else {
        return Ok(None);
    };
    let base_url =
        env_nonempty("NANOGPT_BASE_URL").unwrap_or_else(|| "https://nano-gpt.com/api/v1".into());
    Ok(Some(Llm {
        api_key,
        url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
        model: pick_env(keys, DEFAULT_CHAT_MODEL),
        client: http_client()?,
    }))
}

fn llm_openrouter(keys: &[&str]) -> Result<Option<Llm>, String> {
    let Some(api_key) = env_nonempty("OPENROUTER_API_KEY") else {
        return Ok(None);
    };
    let base_url = env_nonempty("OPENROUTER_BASE_URL")
        .unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
    Ok(Some(Llm {
        api_key,
        url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
        model: pick_env(keys, DEFAULT_CHAT_MODEL),
        client: http_client()?,
    }))
}

fn google_api_key() -> Option<String> {
    env_nonempty("GOOGLE_API_KEY").or_else(|| env_nonempty("GEMINI_API_KEY"))
}

fn llm_google(keys: &[&str]) -> Result<Option<Llm>, String> {
    let Some(api_key) = google_api_key() else {
        return Ok(None);
    };
    let base_url = env_nonempty("GOOGLE_BASE_URL").unwrap_or_else(|| {
        "https://generativelanguage.googleapis.com/v1beta".into()
    });
    let base = base_url.trim_end_matches('/');
    let url = if base.ends_with("/openai") || base.contains("/openai/") {
        format!("{}/chat/completions", base.trim_end_matches("/chat/completions"))
    } else {
        base.to_string()
    };
    Ok(Some(Llm {
        api_key,
        url,
        model: pick_env(keys, DEFAULT_GOOGLE_MODEL),
        client: http_client()?,
    }))
}

fn llm_cerebras(keys: &[&str]) -> Result<Option<Llm>, String> {
    let Some(api_key) = env_nonempty("CEREBRAS_API_KEY") else {
        return Ok(None);
    };
    let base_url = env_nonempty("CEREBRAS_BASE_URL")
        .unwrap_or_else(|| "https://api.cerebras.ai/v1".into());
    Ok(Some(Llm {
        api_key,
        url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
        model: pick_env(keys, "gpt-oss-120b"),
        client: http_client()?,
    }))
}

fn llm_deepseek(keys: &[&str]) -> Result<Option<Llm>, String> {
    let Some(api_key) = env_nonempty("DEEPSEEK_API_KEY") else {
        return Ok(None);
    };
    let base_url = env_nonempty("DEEPSEEK_BASE_URL")
        .unwrap_or_else(|| "https://api.deepseek.com".into());
    Ok(Some(Llm {
        api_key,
        url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
        model: pick_env(keys, DEFAULT_DEEPSEEK_MODEL),
        client: http_client()?,
    }))
}

pub(crate) fn provider_err(
    payload: &Value,
    status: reqwest::StatusCode,
    fallback: &str,
) -> String {
    let msg = payload
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or(fallback);
    let param = payload
        .pointer("/error/param")
        .and_then(Value::as_str)
        .map(|p| format!(" [{p}]"))
        .unwrap_or_default();
    let inner = payload
        .pointer("/error/metadata/raw")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .filter(|s| !s.is_empty() && s != msg)
        .map(|s| format!(": {s}"))
        .unwrap_or_default();
    format!("{msg}{param}{inner} ({status})")
}

fn crew_dir(app: &AppHandle) -> PathBuf {
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crew");
    if from_crate.is_dir() {
        return from_crate;
    }
    if let Ok(cwd) = std::env::current_dir() {
        let here = cwd.join("crew");
        if here.is_dir() {
            return here;
        }
        let up = cwd.join("../crew");
        if up.is_dir() {
            return up;
        }
    }
    // Pacchettizzata: le card stanno nelle resources.
    if let Ok(p) = app
        .path()
        .resolve("crew", tauri::path::BaseDirectory::Resource)
    {
        if p.is_dir() {
            return p;
        }
    }
    from_crate
}

fn parse_card(raw: &str) -> RoleCard {
    let raw = raw.trim();
    let Some(rest) = raw.strip_prefix("---") else {
        return RoleCard {
            tool: None,
            description: None,
            body: raw.to_string(),
        };
    };
    let Some(end) = rest.find("\n---") else {
        return RoleCard {
            tool: None,
            description: None,
            body: raw.to_string(),
        };
    };
    let fm = &rest[..end];
    let body = rest[end + 4..].trim().to_string();
    let mut tool = None;
    let mut description = None;
    for line in fm.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("tool:") {
            tool = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            description = Some(v.trim().to_string());
        }
    }
    RoleCard {
        tool,
        description,
        body,
    }
}

fn load_card(dir: &Path, name: &str) -> Result<RoleCard, String> {
    let path = dir.join(format!("{name}.md"));
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("Missing crew/{name}.md ({e})"))?;
    Ok(parse_card(&raw))
}

fn llm() -> Result<Llm, String> {
    llm_from_env(
        &["NANOGPT_MODEL"],
        &["OPENROUTER_MODEL", "NANOGPT_MODEL"],
        &["GOOGLE_MODEL"],
        &["DEEPSEEK_MODEL"],
        &["CEREBRAS_MODEL"],
    )
}

fn card_tool(card: &RoleCard, fallback_name: &str, fallback_desc: &str) -> Value {
    let name = card
        .tool
        .clone()
        .unwrap_or_else(|| fallback_name.to_string());
    let description = card
        .description
        .clone()
        .unwrap_or_else(|| fallback_desc.to_string());
    let mut tool = json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "brief": {
                        "type": "string",
                        "description": "The User's words, plus only context that is already true: filenames they attached, facts already found. Do not add features, pages, tone, copy, names, or facts they did not write. Do not encode the job as a magic phrase."
                    },
                    "correction": {
                        "type": "boolean",
                        "description": "True when sending this role back to apply findings on work already in the folder. Omit on a first wake."
                    }
                },
                "required": ["brief"]
            }
        }
    });
    tool
}

fn patch_memory_tool(name: &str, description: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "op": { "type": "string", "description": "rewrite (replace the whole file — first look), rewrite_section, replace, delete, or add" },
                    "text": { "type": "string", "description": "For rewrite: the whole markdown file. For add: markdown to append. For rewrite_section: the new body." },
                    "old": { "type": "string", "description": "For replace or delete: an exact unique snippet already in the file." },
                    "new": { "type": "string", "description": "For replace: the new snippet. For rewrite: same as text." },
                    "heading": { "type": "string", "description": "For rewrite_section: a ## heading already in the file." },
                    "ops": {
                        "type": "array",
                        "description": "Several ops in one call, if more than one part of the file must change.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": { "type": "string", "description": "rewrite, rewrite_section, replace, delete, or add" },
                                "text": { "type": "string" },
                                "old": { "type": "string" },
                                "new": { "type": "string" },
                                "heading": { "type": "string" }
                            },
                            "required": ["op"]
                        }
                    }
                },
                "required": ["op"]
            }
        }
    })
}

pub(crate) fn comune_tool() -> Value {
    patch_memory_tool(
        "patch_comune",
        "Update User memory: who they are, what they run, their hours, a number of theirs, how they work, standing preferences — anything about them that should still be true next wake, including what leaked in an order that was otherwise about the world. Also short notes on folders you have worked in (folder name + full path, two to four lines). Not a live number about the world. Not this folder's live picture (.puck/memory.md). Not a diary. Not a task backlog.",
    )
}

pub(crate) fn project_tool() -> Value {
    patch_memory_tool(
        "patch_project",
        "Update this project's memory (.puck/memory.md, inside the open project folder). Call it after a first look at the folder, and after any file change — not empty, not before anyone has looked. Obligatory headings: ## Structure, ## What this is, ## Done, ## Missing. Entries get ids ([m-N]) automatically: add (new bullet), replace (id + new text), remove (id) to delete one, rewrite_section (heading + text), rewrite (whole file, first look or realign). Keep ## What this is to one or two lines: that is the project's identity across the vault. Not two thin lines. Not User prefs. Not a diary.",
    )
}

fn slugify(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else if c.is_whitespace() || c == '-' || c == '_' {
                '-'
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        out = "progetto".into();
    }
    out.chars().take(60).collect()
}

pub(crate) fn open_project_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "open_project",
            "description": "Create or open a project. Every piece of work lives in a project: a folder in the vault with its own memory (.puck/memory.md) and schema (.puck/schema.md). Call it before waking the Coder whenever the order does not already point at the open project: project=\"new: <name>\" creates it (the slug comes from the name), project=\"<slug>\" opens an existing one (list them with look_project what=list). After it returns, the Coder's workspace is that project.",
            "parameters": {
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "\"new: Giardiniere di Monza\" to create, or \"giardiniere-monza\" to open. If new, the project is created ready (memory + schema) and opened."
                    }
                },
                "required": ["project"]
            }
        }
    })
}

pub(crate) fn look_project_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "look_project",
            "description": "Look inside your vault without waking anyone. Use it when you are not sure which project an order belongs to, or before a wake to check what already exists. what=list: every project with its one-line identity and last change. what=schema + slug: the project's .puck/schema.md (the file tree). what=files + slug: top-level listing. what=read + slug + path: a file's content (read-only, up to 2000 lines).",
            "parameters": {
                "type": "object",
                "properties": {
                    "what": {
                        "type": "string",
                        "enum": ["list", "schema", "files", "read"]
                    },
                    "slug": { "type": "string", "description": "Project slug; required except for what=list." },
                    "path": { "type": "string", "description": "File path inside the project, for what=read." }
                },
                "required": ["what"]
            }
        }
    })
}

fn run_open_project(app: &AppHandle, args: &str) -> String {
    let v: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
    let Some(project) = v.get("project").and_then(Value::as_str) else {
        return "open_project needs project=\"new: <name>\" or project=\"<slug>\".".into();
    };
    let project = project.trim().to_string();
    if let Some(name) = project.strip_prefix("new:") {
        let slug = slugify(name.trim());
        return match crate::context::create_project(&slug) {
            Ok(_) => {
                let path_str = crate::context::vault_root()
                    .map(|r| r.join(&slug).display().to_string())
                    .unwrap_or_else(|_| String::new());
                let _ = crate::context::set_workspace(path_str);
                if let Ok(info) = crate::context::get_workspace() {
                    let _ = app.emit("puck-workspace", &info);
                }
                emit_pulse(
                    app,
                    CrewPulse {
                        kind: "workspace",
                        role: Some("coordinatore".into()),
                        from: None,
                        text: Some(slug.clone()),
                        brief: None,
                    },
                );
                format!(
                    "Project created and opened: {slug}. The Coder will work in this project."
                )
            }
            Err(e) => format!("open_project: {e}"),
        };
    }
    match crate::context::project_dir(&project) {
        Ok(p) => {
            let _ = crate::context::set_workspace(p.display().to_string());
            if let Ok(info) = crate::context::get_workspace() {
                let _ = app.emit("puck-workspace", &info);
            }
            emit_pulse(
                app,
                CrewPulse {
                    kind: "workspace",
                    role: Some("coordinatore".into()),
                    from: None,
                    text: Some(project.clone()),
                    brief: None,
                },
            );
            format!("Project opened: {project}. The Coder will work in this project.")
        }
        Err(e) => format!("open_project: {e}"),
    }
}

fn run_look_project(args: &str) -> String {
    let v: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
    let what = v.get("what").and_then(Value::as_str).unwrap_or("");
    let slug = v.get("slug").and_then(Value::as_str).unwrap_or("").trim();
    match what {
        "list" => {
            let Ok(projects) = crate::context::list_projects() else {
                return "Vault: cannot list projects.".into();
            };
            if projects.is_empty() {
                return "No projects yet. Create one with open_project project=\"new: <name>\".".into();
            }
            let mut out = String::from("## Projects\n");
            for p in projects {
                let id = if p.identity.is_empty() {
                    "(no identity yet)"
                } else {
                    p.identity.as_str()
                };
                out.push_str(&format!("- {} — {} (last change {})\n", p.slug, id, p.modified));
            }
            out
        }
        "schema" | "files" => {
            let Ok(p) = crate::context::project_dir(slug) else {
                return format!("look_project: no project {slug}.");
            };
            if what == "schema" {
                match std::fs::read_to_string(p.join(".puck").join("schema.md")) {
                    Ok(s) => return s.if_empty_prefix("(no schema yet)"),
                    Err(_) => return "(no schema yet — open the project and let the Coder work once; the schema is written automatically.".into(),
                }
            }
            let Ok(entries) = std::fs::read_dir(&p) else {
                return format!("look_project: cannot read {slug}.");
            };
            let mut out = String::new();
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || name == "node_modules" {
                    continue;
                }
                if e.path().is_dir() {
                    out.push_str(&format!("{name}/\n"));
                } else {
                    out.push_str(&format!("{name}\n"));
                }
            }
            if out.is_empty() {
                "(empty project)\n".into()
            } else {
                out
            }
        }
        "read" => {
            let Ok(p) = crate::context::project_dir(slug) else {
                return format!("look_project: no project {slug}.");
            };
            let rel = v.get("path").and_then(Value::as_str).unwrap_or("").trim();
            if rel.is_empty() || rel.contains("..") || rel.starts_with('/') {
                return "look_project: bad path.".into();
            }
            let f = p.join(rel);
            match std::fs::read_to_string(&f) {
                Ok(s) => {
                    let mut out = format!("{} ({} lines)\n{}\n", rel, s.lines().count(), s);
                    if out.len() > 200_000 {
                        out.truncate(200_000);
                        out.push_str("\n… (truncated)");
                    }
                    out
                }
                Err(e) => format!("look_project read {rel}: {e}"),
            }
        }
        other => format!("look_project: what must be list, schema, files, or read (got {other})."),
    }
}

trait IfEmptyPrefix {
    fn if_empty_prefix(self, prefix: &str) -> String;
}
impl IfEmptyPrefix for String {
    fn if_empty_prefix(self, prefix: &str) -> String {
        if self.trim().is_empty() {
            format!("{prefix}\n")
        } else {
            self
        }
    }
}

pub(crate) fn apply_comune(ctx: &mut AskCtx<'_>, role: &str, args: &str) -> String {
    match crate::context::patch_comune(args) {
        Ok(msg) => {
            emit_pulse(
                ctx.app,
                CrewPulse {
                    kind: "comune",
                    role: Some(role.to_string()),
                    text: None,
                    brief: None,
                    from: None,
                },
            );
            msg
        }
        Err(e) => {
            crate::context::run_log(
                "comune",
                json!({
                    "ok": false,
                    "role": role,
                    "err": crate::context::clip_log(&e, 180),
                }),
            );
            format!("Common memory: {e}")
        }
    }
}

pub(crate) fn apply_project(ctx: &mut AskCtx<'_>, role: &str, args: &str) -> String {
    let root = match crate::context::workspace_root() {
        Ok(p) => p,
        Err(e) => return format!("Project memory: {e}"),
    };
    match crate::context::patch_project(&root, args) {
        Ok(msg) => {
            emit_pulse(
                ctx.app,
                CrewPulse {
                    kind: "project",
                    role: Some(role.to_string()),
                    text: None,
                    brief: None,
                    from: None,
                },
            );
            msg
        }
        Err(e) => {
            crate::context::run_log(
                "project",
                json!({
                    "ok": false,
                    "role": role,
                    "err": crate::context::clip_log(&e, 180),
                }),
            );
            format!("Project memory: {e}")
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CrewPulse {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    brief: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}

fn emit_pulse(app: &AppHandle, pulse: CrewPulse) {
    let _ = app.emit("puck-crew", pulse);
}

fn clip_words(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out = String::new();
    for word in s.split_whitespace() {
        let next = if out.is_empty() {
            word.to_string()
        } else {
            format!("{out} {word}")
        };
        if next.chars().count() > max {
            break;
        }
        out = next;
    }
    if out.is_empty() {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        format!("{out}…")
    }
}

fn clip_topic(s: &str) -> String {
    clip_words(s, 56)
}

fn clip_announce(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    if s.chars().count() > 320 {
        clip_words(s, 320)
    } else {
        s.to_string()
    }
}

fn echoes_order(spoken: &str, order: &str) -> bool {
    let order = order.split_whitespace().collect::<Vec<_>>().join(" ");
    if order.chars().count() < 24 {
        return false;
    }
    let needle = clip_words(&order, 42)
        .trim_end_matches('…')
        .trim()
        .to_lowercase();
    !needle.is_empty() && spoken.to_lowercase().contains(&needle)
}

fn announce_line(spoken: &str, order: &str) -> String {
    let line = clip_announce(spoken);
    if line.is_empty() || echoes_order(&line, order) {
        String::new()
    } else {
        line
    }
}

fn last_user_order(history: &[ChatTurn]) -> String {
    history
        .iter()
        .rev()
        .find(|turn| turn.role != "assistant")
        .map(|turn| turn.content.trim().to_string())
        .unwrap_or_default()
}

fn pin_owner_order(asked: &str, order: &str, brief: &str) -> String {
    let asked = asked.trim();
    let order = order.trim();
    let brief = brief.trim();
    if asked.is_empty() && order.is_empty() {
        return brief.to_string();
    }
    if brief.starts_with("The User asked:") || brief.starts_with("The Owner asked:") {
        return brief.to_string();
    }
    let origin = if order.is_empty() { asked } else { order };
    let mut out = format!("The User asked:\n{origin}");
    let extra = !brief.is_empty() && brief != origin && brief != asked && brief != order;
    if extra {
        out.push_str("\n\n");
        out.push_str(brief);
    }
    out
}

fn parse_route(raw: &str) -> Result<Option<(String, String)>, ()> {
    let start = raw.find('{').ok_or(())?;
    let end = raw.rfind('}').ok_or(())?;
    let v: Value = serde_json::from_str(&raw[start..=end]).map_err(|_| ())?;
    let tool = match v.get("tool") {
        Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() || s.eq_ignore_ascii_case("null") || s.eq_ignore_ascii_case("none") {
                return Ok(None);
            }
            s
        }
        _ => return Err(()),
    };
    let (id, _) = tool_role(tool).ok_or(())?;
    let brief = v
        .get("brief")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    Ok(Some((id.to_string(), brief)))
}

fn route_prompt(order: &str, spoken: &str, again: bool) -> Vec<Value> {
    let extra = if again {
        " Previous reply was not valid JSON. Reply with one JSON object only."
    } else {
        ""
    };
    vec![
        json!({
            "role": "system",
            "content": "You decide if a trade should have been woken. Reply with one JSON object only. No markdown."
        }),
        json!({
            "role": "user",
            "content": format!(
                "The User's order:\n{order}\n\nThe Coordinator answered in prose and did not call a tool:\n{spoken}\n\nTrades:\n- ask_coder — build or change files. If the outcome is a page or a file, wake only this. That role writes the words too.\n\nJudge the User's job, not the Coordinator's prose, and not the words used. Wake a trade only if that order still needs it.\nIf they are closing or acknowledging work already delivered, or chatting, or the Coordinator started to redo without being asked to change anything, return {{\"tool\":null,\"brief\":\"\"}}.\nIf a trade should have been woken, return {{\"tool\":\"ask_coder\",\"brief\":\"what to pass them\"}}.{extra}"
            )
        }),
    ]
}

async fn infer_missed_tool(
    llm: &Llm,
    order: &str,
    spoken: &str,
) -> Result<Option<(String, String)>, String> {
    let quiet = Some(if llm.is_cerebras() {
        // Cerebras non accetta `thinking` (come `reasoning`): solo effort.
        json!({ "reasoning_effort": "low" })
    } else {
        json!({
            "thinking": { "type": "disabled" },
            "reasoning_effort": "low"
        })
    });
    let message = complete_with(
        llm,
        &route_prompt(order, spoken, false),
        None,
        "none",
        quiet.clone(),
    )
    .await?;
    match parse_route(&message_text(&message)) {
        Ok(route) => Ok(route),
        Err(()) => {
            let retry = complete_with(
                llm,
                &route_prompt(order, spoken, true),
                None,
                "none",
                quiet,
            )
            .await?;
            Ok(parse_route(&message_text(&retry)).unwrap_or(None))
        }
    }
}

pub(crate) fn api_message(message: &Value) -> Value {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant");
    let mut out = json!({ "role": role });
    let content = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(_)) => message_text(message),
        _ => String::new(),
    };
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !calls.is_empty() {
        out["content"] = json!(content);
        out["tool_calls"] = json!(calls);
    } else {
        out["content"] = json!(content);
    }
    if role == "tool" {
        if let Some(id) = message.get("tool_call_id") {
            out["tool_call_id"] = id.clone();
        }
        if let Some(name) = message.get("name") {
            out["name"] = name.clone();
        }
    }
    if let Some(parts) = message.get("gemini_parts") {
        out["gemini_parts"] = parts.clone();
    }
    out
}

fn tool_role(name: &str) -> Option<(&'static str, &'static str)> {
    let n = name.to_lowercase().replace('-', "_");
    if n.contains("coder") {
        Some(("coder", "Coder"))
    } else {
        None
    }
}

pub(crate) async fn complete(
    app: &AppHandle,
    role: &str,
    llm: &Llm,
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &str,
) -> Result<Value, String> {
    crate::stream::complete_live(app, role, llm, messages, tools, tool_choice).await
}

pub(crate) async fn complete_tokens(
    llm: &Llm,
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &str,
) -> Result<Value, String> {
    complete_with(
        llm,
        messages,
        tools,
        tool_choice,
        Some(llm.thinking_extra_for("coordinatore")),
    )
    .await
}

pub(crate) async fn complete_with(
    llm: &Llm,
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &str,
    extra: Option<Value>,
) -> Result<Value, String> {
    // No Puck ceiling. Send the model's own maximum so a gateway cannot
    // default to 4096 and clip thinking plus the piece. Gemini: 65k. DeepSeek V4: 384k.
    let pruned = crate::context::prune_for_model(messages, llm.sees_images());
    if llm.uses_native_gemini() {
        return match crate::provider::gemini_complete(
            llm,
            &pruned,
            tools,
            tool_choice,
            extra.as_ref(),
            "coordinatore",
        )
        .await
        {
            Ok((message, usage)) => {
                let names: Vec<String> = tool_calls(&message)
                    .iter()
                    .filter_map(|c| {
                        c.pointer("/function/name")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                    })
                    .collect();
                crate::context::run_log(
                    "llm",
                    json!({
                        "ok": true,
                        "model": llm.model,
                        "via": "gemini",
                        "tools": names,
                        "usage": crate::provider::log_usage(&usage),
                        "text": crate::context::clip_log(&message_text(&message), 120),
                    }),
                );
                Ok(message)
            }
            Err(err) => {
                crate::context::run_log(
                    "llm",
                    json!({
                        "ok": false,
                        "model": llm.model,
                        "via": "gemini",
                        "err": crate::context::clip_log(&err, 360),
                    }),
                );
                Err(err)
            }
        };
    }
    let mut body = json!({
        "model": llm.model,
        "messages": pruned,
        "max_tokens": llm.max_tokens(),
        "stream": false
    });
    if tools.is_some() {
        body["tool_choice"] = json!(tool_choice);
    }
    with_prompt_cache(&mut body, tools, "");
    if let Some(Value::Object(map)) = extra {
        for (k, v) in map {
            body[k] = v;
        }
    }
    llm.prepare_body(&mut body);

    let response = llm
        .bind(llm.client.post(&llm.url))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            let err = format!("Network: {}", llm_http_err(e));
            crate::context::run_log(
                "llm",
                json!({
                    "ok": false,
                    "model": llm.model,
                    "err": crate::context::clip_log(&err, 200),
                }),
            );
            err
        })?;

    let status = response.status();
    let payload: Value = response.json().await.map_err(|e| {
        let err = format!("Bad JSON from NanoGPT: {}", llm_http_err(e));
        crate::context::run_log(
            "llm",
            json!({
                "ok": false,
                "model": llm.model,
                "err": crate::context::clip_log(&err, 200),
            }),
        );
        err
    })?;

    if !status.is_success() {
        let err = provider_err(&payload, status, "LLM request failed");
        crate::context::run_log(
            "llm",
            json!({
                "ok": false,
                "model": llm.model,
                "err": crate::context::clip_log(&err, 360),
            }),
        );
        return Err(err);
    }

    let message = payload
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| {
            crate::context::run_log(
                "llm",
                json!({
                    "ok": false,
                    "model": llm.model,
                    "err": "NanoGPT returned no message.",
                }),
            );
            "NanoGPT returned no message.".to_string()
        })?;
    let names: Vec<String> = tool_calls(&message)
        .iter()
        .filter_map(|c| {
            c.pointer("/function/name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect();
    let usage = crate::provider::usage_from_openai(&payload);
    crate::context::run_log(
        "llm",
        json!({
            "ok": true,
            "model": llm.model,
            "tools": names,
            "usage": crate::provider::log_usage(&usage),
            "text": crate::context::clip_log(&message_text(&message), 120),
        }),
    );
    Ok(message)
}

pub(crate) fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

fn message_reason(message: &Value) -> String {
    message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

pub(crate) fn spoken_content(message: &Value) -> String {
    let spoken = message_text(message);
    if crate::stream::is_reasoning_echo(&spoken, &message_reason(message)) {
        return String::new();
    }
    spoken
}

pub(crate) fn tool_calls(message: &Value) -> Vec<Value> {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn spoken_for_titolare(spoken: String, work: &[WorkPiece]) -> String {
    let spoken = spoken.trim().to_string();
    if spoken.is_empty() {
        return String::new();
    }
    let mut out = spoken;
    // Il recap del Coder è interno: se il Coordinatore l'ha ripetuto nella
    // risposta al Titolare (nudo, tra backtick o dentro un fence), si toglie.
    // Il Titolare riceve solo il report; niente fence copiabile dal recap.
    for piece in work {
        let recap = piece.text.trim();
        if recap.is_empty() || piece.role != "coder" {
            continue;
        }
        out = strip_recap_echo(out, recap);
    }
    // Il fence-ripulino gira anche senza pezzi del Coder (il Coordinatore
    // può scriversi da solo una riga "file.ext — …" in un fence).
    out = strip_recap_fences(out);
    // "Fatto." in testa è il prefisso che il prompt vieta: si toglie.
    for prefix in ["Fatto. ", "Fatto! ", "Fatto.  ", "Fatto!"] {
        if let Some(rest) = out.strip_prefix(prefix) {
            out = rest.trim_start().to_string();
            break;
        }
    }
    out.trim().to_string()
}

fn strip_recap_echo(mut s: String, recap: &str) -> String {
    let wrapped = [
        format!("```\n{}\n```", recap),
        format!("```{}```", recap),
        format!("```\n{}```", recap),
        format!("```{}\n```", recap),
        format!("`{}`", recap),
    ];
    for w in &wrapped {
        s = s.replace(w, "");
    }
    s = s.replace(recap, "");
    // Residui: fence rimaste vuote e righe doppie. Le fence con contenuto
    // proprio (un pezzo standalone da copiare) restano.
    loop {
        let before = s.len();
        s = s
            .replace("```\n\n```", "")
            .replace("```\n ```", "")
            .replace("```\n\n ```", "")
            .replace("\n\n\n", "\n\n")
            .replace("  ", " ");
        s = s.trim().to_string();
        if s.len() == before {
            break;
        }
    }
    strip_recap_fences(s)
}

/// Un fence che contiene una riga sola "file.ext — …" è un recap del Coder
/// ripetuto dal Coordinatore (anche parafrasato): si toglie il blocco intero.
/// Un pezzo standalone da copiare (messaggio, mail, titolo) non inizia così.
fn strip_recap_fences(mut s: String) -> String {
    let mut out = String::new();
    let mut rest = s.as_str();
    while let Some(start) = rest.find("```") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        match after.find("```") {
            Some(end) => {
                let inner = after[..end].trim();
                if is_recap_file_line(inner) {
                    // blocco-appello del recap: via, insieme alle righe vuote dietro
                    while out.ends_with('\n') {
                        out.pop();
                    }
                } else {
                    out.push_str("```");
                    out.push_str(&after[..end]);
                    out.push_str("```");
                }
                rest = &after[end + 3..];
            }
            None => {
                out.push_str("```");
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_recap_file_line(s: &str) -> bool {
    if s.is_empty() || s.contains('\n') {
        return false;
    }
    let Some((name, _)) = s.split_once(" —").or_else(|| s.split_once(":")) else {
        return false;
    };
    let name = name.trim().trim_end_matches(['.', ' ', '`', '*']);
    ["html", "css", "js", "json", "md", "txt", "svg", "png", "jpg", "jpeg", "webp", "jsonl"]
        .iter()
        .any(|ext| name.ends_with(&format!(".{ext}")))
}

pub(crate) fn brief_from_args(args: &str) -> String {
    serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|v| {
            v.get("brief")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| args.trim().to_string())
}

pub(crate) fn correction_from_args(args: &str) -> bool {
    serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|v| v.get("correction").and_then(Value::as_bool))
        .unwrap_or(false)
}

pub(crate) fn paths_from_args(args: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(args) else {
        return Vec::new();
    };
    match v.get("paths") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| item.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                Vec::new()
            } else {
                vec![t.to_string()]
            }
        }
        _ => Vec::new(),
    }
}

const MAX_CHAIN: usize = 3;

pub(crate) struct AskCtx<'a> {
    pub app: &'a AppHandle,
    pub llm: &'a Llm,
    pub coder_llm: &'a Llm,
    pub asked: &'a str,
    pub order: &'a str,
    pub memory: &'a Mutex<Memory>,
    pub live_threads: &'a mut HashMap<String, Vec<ChatTurn>>,
    pub work: &'a mut Vec<WorkPiece>,
    pub woke: &'a mut Vec<String>,
    pub coder: &'a RoleCard,
    pub stack: &'a mut Vec<String>,
}

pub(crate) fn can_ask(from: &str, to: &str) -> bool {
    if from == to {
        return false;
    }
    match from {
        "coordinatore" => to == "coder",
        _ => false,
    }
}

fn colleague_wrap(_id: &str, text: &str) -> String {
    match _id {
        _ => text.to_string(),
    }
}

pub(crate) fn ask_from_role<'a>(
    ctx: &'a mut AskCtx<'_>,
    from: &'a str,
    id: &'a str,
    brief: String,
    correction: bool,
    paths: Vec<String>,
) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
    Box::pin(async move {
        if !can_ask(from, id) {
            crate::context::run_log(
                "wake_blocked",
                json!({ "from": from, "to": id, "reason": "cannot_ask" }),
            );
            return format!("You cannot call {id}.");
        }
        if ctx.stack.iter().any(|s| s == id) {
            crate::context::run_log(
                "wake_blocked",
                json!({ "from": from, "to": id, "reason": "loop" }),
            );
            return format!("{id} is already in this chain. Do not loop.");
        }
        if ctx.stack.len() >= MAX_CHAIN {
            crate::context::run_log(
                "wake_blocked",
                json!({ "from": from, "to": id, "reason": "chain_full" }),
            );
            return "The chain is full. Use what you have and finish your own job.".into();
        }
        ctx.stack.push(id.to_string());
        let wrap = wake_trade(ctx, from, id, brief, correction, paths).await;
        ctx.stack.pop();
        wrap
    })
}

fn tool_call_id(call: &Value) -> &str {
    call.get("id").and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn tool_call_name(call: &Value) -> &str {
    call.pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn tool_call_args(call: &Value) -> &str {
    call.pointer("/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or("")
}

pub(crate) fn is_people_tool(name: &str) -> bool {
    matches!(
        name,
        "ask_coder"
    )
}

pub(crate) fn split_calls(calls: Vec<Value>) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let mut rest = Vec::new();
    let mut comune = Vec::new();
    let mut people = Vec::new();
    for call in calls {
        let name = tool_call_name(&call);
        if name == "patch_comune" || name == "patch_project" {
            comune.push(call);
        } else if is_people_tool(name) {
            people.push(call);
        } else {
            rest.push(call);
        }
    }
    (rest, comune, people)
}

fn push_tool(messages: &mut Vec<Value>, id: &str, content: String) {
    messages.push(json!({
        "role": "tool",
        "tool_call_id": id,
        "content": content
    }));
}

fn append_last_tool(messages: &mut [Value], extra: &str) {
    let Some(last) = messages.last_mut() else {
        return;
    };
    if last.get("role").and_then(Value::as_str) != Some("tool") {
        return;
    }
    let Some(content) = last.get("content").and_then(Value::as_str) else {
        return;
    };
    last["content"] = json!(format!("{content} {extra}"));
}

async fn run_comune_calls(
    ctx: &mut AskCtx<'_>,
    role: &str,
    calls: &[Value],
    messages: &mut Vec<Value>,
) -> (bool, bool) {
    let mut comune_ok = false;
    let mut project_ok = false;
    for call in calls {
        let name = tool_call_name(call);
        let result = if name == "patch_project" {
            apply_project(ctx, role, tool_call_args(call))
        } else {
            apply_comune(ctx, role, tool_call_args(call))
        };
        if result.starts_with("Common memory updated") {
            comune_ok = true;
        }
        if result.starts_with("Project memory updated") {
            project_ok = true;
        }
        push_tool(messages, tool_call_id(call), result);
    }
    if comune_ok {
        match compact_comune_if_needed(ctx.app, ctx.llm, role).await {
            Ok(true) => append_last_tool(
                messages,
                "It went over the cap, so it was compacted to the current picture. Continue from here.",
            ),
            Err(e) => append_last_tool(
                messages,
                &format!("Compact failed ({e}). The file is still the long version."),
            ),
            Ok(false) => {}
        }
    }
    if project_ok {
        if let Ok(root) = crate::context::workspace_root() {
            match compact_project_if_needed(ctx.app, ctx.llm, role, &root).await {
                Ok(true) => append_last_tool(
                    messages,
                    "Project memory went over the cap, so it was compacted. Continue from here.",
                ),
                Err(e) => append_last_tool(
                    messages,
                    &format!("Project compact failed ({e}). The file is still the long version."),
                ),
                Ok(false) => {}
            }
        }
    }
    (comune_ok, project_ok)
}

fn copy_read_file(ctx: &AskCtx<'_>, allowed: &[String], args: &str) -> String {
    if allowed.is_empty() {
        return "This job has no files. Write the standalone piece. Do not read.".into();
    }
    let v: Value = serde_json::from_str(args).unwrap_or(json!({}));
    let path = v
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if path.is_empty() {
        return "Missing path.".into();
    }
    let Ok(root) = crate::coder::workspace_root() else {
        return "No folder is chosen.".into();
    };
    match crate::coder::path_is_allowed(&root, path, allowed) {
        Ok(false) => format!(
            "That file is not in this job. Read only: {}.",
            allowed.join(", ")
        ),
        Err(e) => e,
        Ok(true) => {
            emit_pulse(
                ctx.app,
                CrewPulse {
                    kind: "read",
                    role: Some("coder".into()),
                    text: Some(path.to_string()),
                    brief: None,
                    from: None,
                },
            );
            let offset = v.get("offset").and_then(Value::as_u64);
            match crate::coder::read_file(&root, path, offset, None) {
                Ok(body) => body,
                Err(e) => e,
            }
        }
    }
}


fn coder_landed(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim();
        t.starts_with("Wrote:") || t.starts_with("Deleted:") || t.starts_with("Moved:")
    })
}

fn coder_built(text: &str) -> bool {
    text.lines().any(|l| l.trim().starts_with("Wrote:"))
}

fn recap_only_rule() -> &'static str {
    "Your lines to the User come only from the recap below. Do not add prices, counts, names, or facts that are not in it. If a number is not in the recap, omit it."
}

fn coder_wrap(text: &str, calls: usize) -> String {
    let rule = recap_only_rule();
    if calls >= 3 {
        format!(
            "Notes from the Coder. {rule} One or two short lines from the recap. Do not paste the file. Do not list the tools.\n\n{text}"
        )
    } else if coder_landed(text) {
        format!(
            "Notes from the Coder.\n\n{rule}\nThere is a Wrote:, Deleted:, or Moved: line — it landed. The lines after that are their recap. One or two short lines to the User from that recap — what they got, that it is done. Do not paste the file. Do not list the tools. If they wrote HTML this job they already opened it.\nIf the write is broken (prose instead of a tool, wandered off, off the brief), send them back once with correction true. Do not invent extra changes.\n\n{text}"
        )
    } else {
        format!(
            "Notes from the Coder.\n\n{rule}\nIf the User asked them to look or report, this recap is the answer — do not send them back to write a file. Do not invent a price or a count. If the User asked them to write and there is no Wrote: line, send them back to call a file tool.\n\n{text}"
        )
    }
}

fn land_work(
    ctx: &mut AskCtx<'_>,
    from: &str,
    id: &str,
    brief: &str,
    text: String,
    turns: Vec<ChatTurn>,
    paths: Vec<String>,
) {
    if !ctx.woke.iter().any(|r| r == id) {
        ctx.woke.push(id.to_string());
    }
    ctx.work.push(WorkPiece {
        role: id.to_string(),
        brief: brief.to_string(),
        text: text.clone(),
        from: from.to_string(),
        paths,
    });
    let mut saved = turns;
    saved.push(ChatTurn {
        role: "assistant".into(),
        content: text.clone(),
    });
    save_turns(id, &saved);
    ctx.live_threads.insert(id.to_string(), saved);
    emit_pulse(
        ctx.app,
        CrewPulse {
            kind: "heard",
            role: Some(id.to_string()),
            text: Some(text.clone()),
            brief: None,
            from: Some(from.to_string()),
        },
    );
    crate::context::run_log(
        "land",
        json!({
            "from": from,
            "role": id,
            "out": crate::context::clip_log(&text, 160),
        }),
    );
}

const CODER_WAKE_CAP: usize = 2;

fn coder_wake_cap(from: &str, id: &str, work: &[WorkPiece]) -> Option<&'static str> {
    if from != "coordinatore" || id != "coder" {
        return None;
    }
    let n = work.iter().filter(|p| p.role == "coder").count();
    if n >= CODER_WAKE_CAP {
        Some("You already woke the Coder twice this order. Report to the User from the last recap. Do not call ask_coder again.")
    } else {
        None
    }
}

fn wrap_for(from: &str, id: &str, text: &str, n: usize, _paths: &[String]) -> String {
    if from != "coordinatore" {
        return colleague_wrap(id, text);
    }
    match id {
        "coder" => coder_wrap(text, n),
        _ => text.to_string(),
    }
}

async fn wake_trade(
    ctx: &mut AskCtx<'_>,
    from: &str,
    id: &str,
    brief: String,
    correction: bool,
    paths: Vec<String>,
) -> String {
    if let Some(msg) = coder_wake_cap(from, id, ctx.work) {
        crate::context::run_log(
            "wake_blocked",
            json!({ "from": from, "to": id, "reason": "coder_cap" }),
        );
        return msg.into();
    }
    let brief = pin_owner_order(ctx.asked, ctx.order, &brief);
    crate::context::run_log(
        "wake",
        json!({
            "from": from,
            "to": id,
            "brief": crate::context::clip_log(&brief, 160),
        }),
    );
    emit_pulse(
        ctx.app,
        CrewPulse {
            kind: "talk",
            role: Some(id.to_string()),
            text: None,
            brief: Some(brief.clone()),
            from: Some(from.to_string()),
        },
    );

    let wrap = match id {
        "coder" => {
            let mut role_turns = ctx.live_threads.get("coder").cloned().unwrap_or_default();
            if role_turns.last().map(|t| t.content.as_str()) != Some(brief.as_str()) {
                role_turns.push(ChatTurn {
                    role: "user".into(),
                    content: brief.clone(),
                });
            }
            match pack_thread(ctx.app, ctx.llm, ctx.memory, "coder", &role_turns).await {
                Err(e) => format!("Coder failed: {e}"),
                Ok(packed) => {
                    let app = ctx.app.clone();
                    let coder_llm = ctx.coder_llm.clone();
                    let body = ctx.coder.body.clone();
                    match crate::coder::run_coder(
                        &app,
                        &coder_llm,
                        &body,
                        &packed,
                        Some(ctx),
                        correction,
                    )
                    .await {
                        Ok(text) => {
                            land_work(
                                ctx,
                                from,
                                "coder",
                                &brief,
                                text.clone(),
                                role_turns,
                                Vec::new(),
                            );
                            let n = ctx.work.iter().filter(|p| p.role == "coder").count();
                            wrap_for(from, "coder", &text, n, &[])
                        }
                        Err(e) => format!("Coder failed: {e}"),
                    }
                }
            }
        }
        other => format!("Unknown tool: {other}"),
    };

    if wrap.contains(" failed:") || wrap.starts_with("Unknown tool:") {
        crate::context::run_log(
            "wake_fail",
            json!({
                "from": from,
                "to": id,
                "err": crate::context::clip_log(&wrap, 180),
            }),
        );
    }
    emit_pulse(
        ctx.app,
        CrewPulse {
            kind: "idle",
            role: Some(id.to_string()),
            text: None,
            brief: None,
            from: Some(from.to_string()),
        },
    );
    wrap
}

#[tauri::command]
pub async fn ask_coordinatore(
    app: AppHandle,
    memory: State<'_, Mutex<Memory>>,
    history: Vec<ChatTurn>,
    awake: Vec<String>,
    crew_threads: Option<HashMap<String, Vec<ChatTurn>>>,
) -> Result<AskReply, String> {
    if history.is_empty() {
        return Err("Empty order.".into());
    }
    // Puck Cloud: check di allineamento a inizio task (pull se il remoto è più nuovo).
    let _ = crate::cloud::check_app(&app).await;
    crate::context::run_log(
        "order",
        json!({
            "text": crate::context::clip_log(&last_user_order(&history), 200),
        }),
    );

    let llm = llm()?;
    let coder_llm = crate::coder::llm_coder()?;
    let dir = crew_dir(&app);
    let coordinatore = load_card(&dir, "coordinatore")?;
    let coder = load_card(&dir, "coder")?;
    let tools = vec![
        card_tool(
            &coder,
            "ask_coder",
            "Wake the Coder to build or change files. Call this in the same turn you say they are working. Saying they are working is not a wake. They own the files, including the words and how it looks. Pass the User's order. If their recap skipped a file point or answered it with a proxy, wake them again with only what is missing — do not change the subject first.",
        ),
        crate::ask::tool(),
        comune_tool(),
        project_tool(),
        open_project_tool(),
        look_project_tool(),
    ];
    let history = absorb_thread("coordinatore", history);
    let mut crew_threads = crew_threads.unwrap_or_default();
    let ids: Vec<String> = crew_threads.keys().cloned().collect();
    for id in ids {
        if let Some(turns) = crew_threads.remove(&id) {
            crew_threads.insert(id.clone(), absorb_thread(&id, turns));
        }
    }
    fill_threads_from_disk(&mut crew_threads);

    let packed_main = pack_thread(&app, &llm, &*memory, "coordinatore", &history).await?;
    let standing = coordinatore.body.clone();
    let summary = packed_main.summary.clone();

    let mut messages = system_turns(&standing, &summary);
    for turn in &packed_main.keep {
        let role = match turn.role.as_str() {
            "assistant" => "assistant",
            _ => "user",
        };
        messages.push(json!({
            "role": role,
            "content": turn.content,
        }));
    }

    let mut live_threads = crew_threads;
    let mut woke: Vec<String> = Vec::new();
    let mut work: Vec<WorkPiece> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut final_text = String::new();
    let order = last_user_order(&history);
    let asked = order.clone();
    let mut inferred = false;

    for _ in 0..14 {
        crate::todo::pin_live(
            &mut messages,
            Some(&current_awake(&awake, &woke)),
            &[],
        );
        let message = complete(&app, "coordinatore", &llm, &messages, Some(&tools), "auto").await?;
        let calls = tool_calls(&message);
        if calls.is_empty() {
            let raw = message_text(&message);
            let spoken = spoken_content(&message);
            if !spoken.is_empty() {
                emit_pulse(
                    &app,
                    CrewPulse {
                        kind: "say",
                        role: Some("coordinatore".into()),
                        text: Some(spoken.clone()),
                        brief: None,
                        from: None,
                    },
                );
            }
            if !inferred && work.is_empty() {
                inferred = true;
                match infer_missed_tool(&llm, &order, &raw).await {
                    Ok(Some((id, brief))) => {
                        let line = announce_line(&spoken, &order);
                        if !line.is_empty() {
                            emit_pulse(
                                &app,
                                CrewPulse {
                                    kind: "say",
                                    role: None,
                                    text: Some(line),
                                    brief: None,
                                    from: None,
                                },
                            );
                        }
                        let brief = if brief.is_empty() {
                            order.clone()
                        } else {
                            brief
                        };
                        let wrap = {
                            let mut ctx = AskCtx {
                                app: &app,
                                llm: &llm,
                                coder_llm: &coder_llm,
                                asked: &asked,
                                order: &order,
                                memory: &*memory,
                                live_threads: &mut live_threads,
                                work: &mut work,
                                woke: &mut woke,
                                coder: &coder,
                                stack: &mut stack,
                            };
                            ask_from_role(&mut ctx, "coordinatore", &id, brief, false, Vec::new()).await
                        };
                        messages.push(api_message(&message));
                        messages.push(json!({
                            "role": "user",
                            "content": wrap
                        }));
                        continue;
                    }
                    _ => {
                        final_text = spoken;
                        break;
                    }
                }
            }
            final_text = spoken;
            break;
        }

        let spoken = message_text(&message);
        let mut said: Vec<&str> = Vec::new();
        let mut announced = false;
        for call in &calls {
            let name = tool_call_name(call);
            if let Some((_, who)) = tool_role(name) {
                if !said.contains(&who) {
                    said.push(who);
                    if !announced {
                        announced = true;
                        let line = announce_line(&spoken, &order);
                        if !line.is_empty() {
                            emit_pulse(
                                &app,
                                CrewPulse {
                                    kind: "say",
                                    role: None,
                                    text: Some(line),
                                    brief: None,
                                    from: None,
                                },
                            );
                        }
                    }
                }
            }
        }

        messages.push(api_message(&message));

        let (rest, comune_calls, people) = split_calls(calls);
        for call in rest {
            let name = tool_call_name(&call);
            let result = if name == "ask_user" {
                crate::ask::run(&app, "coordinatore", tool_call_args(&call)).await
            } else if name == "open_project" {
                run_open_project(&app, tool_call_args(&call))
            } else if name == "look_project" {
                run_look_project(tool_call_args(&call))
            } else {
                format!("Unknown tool: {name}")
            };
            push_tool(&mut messages, tool_call_id(&call), result);
        }
        {
            let mut ctx = AskCtx {
                app: &app,
                llm: &llm,
                coder_llm: &coder_llm,
                asked: &asked,
                order: &order,
                memory: &*memory,
                live_threads: &mut live_threads,
                work: &mut work,
                woke: &mut woke,
                coder: &coder,
                stack: &mut stack,
            };
            let _ = run_comune_calls(
                &mut ctx,
                "coordinatore",
                &comune_calls,
                &mut messages,
            )
            .await;
        }
        for call in people {
            let name = tool_call_name(&call);
            let args = tool_call_args(&call);
            let brief = brief_from_args(args);
            let correction = correction_from_args(args);
            let paths = paths_from_args(args);
            let result = match tool_role(name).map(|(id, _)| id) {
                Some(id) => {
                    let mut ctx = AskCtx {
                        app: &app,
                        llm: &llm,
                        coder_llm: &coder_llm,
                        asked: &asked,
                        order: &order,
                        memory: &*memory,
                        live_threads: &mut live_threads,
                        work: &mut work,
                        woke: &mut woke,
                        coder: &coder,
                        stack: &mut stack,
                    };
                    ask_from_role(&mut ctx, "coordinatore", id, brief, correction, paths).await
                }
                None => format!("Unknown tool: {name}"),
            };
            push_tool(&mut messages, tool_call_id(&call), result);
        }
    }

    let text = spoken_for_titolare(final_text, &work);
    persist_after_ask("coordinatore", &history, &text, &live_threads);
    crate::context::run_log(
        "order_end",
        json!({
            "woke": woke,
            "work": work.iter().map(|p| p.role.clone()).collect::<Vec<_>>(),
            "reply": crate::context::clip_log(&text, 160),
        }),
    );
    if text.is_empty() && work.is_empty() {
        return Err("The Coordinator kept calling tools and never reported back.".into());
    }

    // Puck Cloud: push a fine messaggio (se configurato; best-effort).
    let _ = crate::cloud::push_app(&app).await;

    Ok(AskReply {
        text,
        woke,
        work,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        announce_line, can_ask, chat_backend_from, coder_built, coder_wake_cap, coder_wrap, comune_tool,
        correction_from_args, is_timeout_msg, mark_tools_cached, message_text,
        paths_from_args, pin_owner_order, provider_err, role_system,
        spoken_content, spoken_for_titolare, system_turns, with_prompt_cache,
        ChatBackend, Llm, WorkPiece,
    };
    use serde_json::json;

    #[test]
    fn cerebras_adapter_shape() {
        let llm = Llm {
            api_key: "csk-x".into(),
            url: "https://api.cerebras.ai/v1/chat/completions".into(),
            model: "gpt-oss-120b".into(),
            client: reqwest::Client::new(),
        };
        assert!(llm.is_cerebras());
        assert_eq!(llm.max_tokens(), 32_768);
        let e = llm.extra_for_effort("auto");
        assert!(e.get("reasoning_effort").is_some());
        assert!(e.get("reasoning").is_none());
        assert!(!llm.sees_images());
    }

    fn dummy_llm(model: &str) -> Llm {
        Llm {
            api_key: "x".into(),
            url: "https://nano-gpt.com/api/v1/chat/completions".into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }

    #[test]
    fn spoken_content_drops_reasoning_echo() {
        let thought = "**Reviewing New Content**\n\nI've received the colleague's latest submission.";
        let echo = json!({
            "content": thought,
            "reasoning": thought,
            "reasoning_content": thought
        });
        assert_eq!(spoken_content(&echo), "");
        assert_eq!(message_text(&echo), thought);
        let real = json!({
            "content": "Manca il nome del destinatario.",
            "reasoning": "**Drafting**\n\nKeep the recap short."
        });
        assert_eq!(spoken_content(&real), "Manca il nome del destinatario.");
    }

    #[test]
    fn spoken_for_titolare_keeps_the_spoken_text() {
        assert_eq!(spoken_for_titolare("Ciao.".into(), &[]), "Ciao.");
        assert!(crate::coordinatore::spoken_for_titolare("Manca.".into(), &[]).contains("Manca."));
    }

    #[test]
    fn paths_from_args_reads_list_or_one() {
        assert_eq!(
            paths_from_args(r#"{"brief":"x","paths":["index.html","./about.html"]}"#),
            vec!["index.html", "./about.html"]
        );
        assert_eq!(
            paths_from_args(r#"{"brief":"x","paths":"only.html"}"#),
            vec!["only.html"]
        );
        assert!(paths_from_args(r#"{"brief":"hello"}"#).is_empty());
    }
    #[test]
    fn system_turns_cache_standing_not_summary() {
        let turns = system_turns("Be brief.", "yesterday's job");
        assert_eq!(turns.len(), 2);
        let parts = turns[0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["cache_control"]["ttl"], json!("1h"));
        assert!(parts[0]["text"].as_str().unwrap().contains("Be brief."));
        assert!(turns[1]["content"]
            .as_str()
            .unwrap()
            .contains("yesterday's job"));
        let only = system_turns("Be brief.", "  ");
        assert_eq!(only.len(), 1);
        assert!(role_system("Be brief.", "x").contains("## This thread"));
        let tools = mark_tools_cached(&[json!({"type":"function","function":{"name":"a"}})]);
        assert_eq!(tools[0]["cache_control"]["ttl"], json!("1h"));
        let mut body = json!({"model": "x"});
        with_prompt_cache(&mut body, None, "puck-coder");
        assert_eq!(body["prompt_caching"]["ttl"], json!("1h"));
        assert_eq!(body["prompt_cache_key"], json!("puck-coder"));
    }
    #[test]
    fn trades_can_call_along_the_job() {
        assert!(can_ask("coordinatore", "coder"));
        assert!(!can_ask("coder", "coder"));
        assert!(!can_ask("coordinatore", "coordinatore"));
    }

    #[test]
    fn comune_ops_array_declares_items() {
        let tool = comune_tool();
        let items = tool.pointer("/function/parameters/properties/ops/items");
        assert_eq!(items.and_then(|v| v.get("type")), Some(&json!("object")));
    }

    #[test]
    fn puck_chat_picks_google() {
        assert_eq!(chat_backend_from("google"), ChatBackend::Google);
        assert_eq!(chat_backend_from("GEMINI"), ChatBackend::Google);
        assert_eq!(chat_backend_from("openrouter"), ChatBackend::OpenRouter);
        assert_eq!(chat_backend_from("nano"), ChatBackend::Nano);
        assert_eq!(chat_backend_from(""), ChatBackend::Nano);
    }

    #[test]
    fn google_prepare_body_strips_compat_extras() {
        let llm = Llm {
            api_key: "x".into(),
            url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
                .into(),
            model: "gemini-3.7-flash".into(),
            client: reqwest::Client::new(),
        };
        assert!(llm.is_google());
        assert_eq!(
            llm.thinking_extra_for("coder")["extra_body"]["google"]["thinking_config"]
                ["thinking_level"],
            json!("HIGH")
        );
        assert_eq!(
            llm.thinking_extra_for("coordinatore")["extra_body"]["google"]["thinking_config"]
                ["thinking_level"],
            json!("MEDIUM")
        );
        let mut body = json!({
            "reasoning_effort": "high",
            "reasoning": { "effort": "high", "enabled": true },
            "prompt_caching": { "ttl": "1h" },
            "prompt_cache_key": "puck-coder",
            "tools": [{ "type": "function", "cache_control": { "type": "ephemeral" } }],
            "messages": [{
                "role": "system",
                "content": [{ "type": "text", "text": "hi", "cache_control": { "ttl": "1h" } }]
            }]
        });
        llm.prepare_body(&mut body);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("prompt_caching").is_none());
        assert!(body.get("prompt_cache_key").is_none());
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert!(body["tools"][0].get("cache_control").is_none());
        assert!(body["messages"][0]["content"][0].get("cache_control").is_none());
        assert!(!dummy_llm("gemini-3.7-flash").is_google());
    }

    #[test]
    fn deepseek_api_prepares_clean_body_and_sees_images() {
        let llm = Llm {
            api_key: "x".into(),
            url: "https://api.deepseek.com/chat/completions".into(),
            model: "deepseek-v4-flash-vision-exp".into(),
            client: reqwest::Client::new(),
        };
        assert!(llm.is_deepseek_api());
        assert!(llm.sees_images());
        let mut body = json!({
            "reasoning_effort": "high",
            "reasoning": { "effort": "high", "enabled": true },
            "prompt_caching": { "ttl": "1h" },
            "prompt_cache_key": "puck-coder",
            "thinking": { "type": "enabled" },
            "tools": [{ "type": "function", "cache_control": { "type": "ephemeral" } }]
        });
        llm.prepare_body(&mut body);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("prompt_caching").is_none());
        assert!(body.get("prompt_cache_key").is_none());
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert!(body["tools"][0].get("cache_control").is_none());
    }

    #[test]
    fn deepseek_api_effort_follows_official_ladder() {
        let llm = Llm {
            api_key: "x".into(),
            url: "https://api.deepseek.com/chat/completions".into(),
            model: "deepseek-v4-flash-vision-exp".into(),
            client: reqwest::Client::new(),
        };
        // Official mapping: medium -> high, high -> high, xhigh -> high.
        for role in ["coordinatore", "coder"] {
            let extra = llm.thinking_extra_for(role);
            assert_eq!(extra["reasoning_effort"], json!("high"));
            assert!(extra.get("reasoning").is_none());
            assert_eq!(extra["thinking"]["type"], json!("enabled"));
        }
    }

    #[test]
    fn puck_chat_picks_deepseek() {
        assert_eq!(chat_backend_from("deepseek"), ChatBackend::DeepSeek);
    }

    #[test]
    fn grok_asks_xhigh_thinking() {
        let extra = dummy_llm("x-ai/grok-4.6").thinking_extra();
        assert_eq!(extra["reasoning_effort"], json!("high"));
        assert_eq!(extra["reasoning"]["effort"], json!("high"));
        assert!(!dummy_llm("x-ai/grok-4.6").is_gemini());
        assert!(dummy_llm("x-ai/grok-4.6").is_grok());
        let gemini = dummy_llm("google/gemini-3.7-flash").thinking_extra();
        assert_eq!(gemini["reasoning_effort"], json!("high"));
        let ds = dummy_llm("deepseek/deepseek-v4-flash-0731:thinking").thinking_extra();
        assert_eq!(ds["reasoning_effort"], json!("high"));
        assert_eq!(ds["reasoning"]["effort"], json!("high"));
        assert_eq!(
            dummy_llm("deepseek/deepseek-v4-pro-0813:thinking").thinking_extra()["reasoning_effort"],
            json!("high")
        );
        let glm = dummy_llm("zai-org/glm-5.2:thinking").thinking_extra();
        assert_eq!(glm["reasoning_effort"], json!("high"));
        assert_eq!(glm["reasoning"]["effort"], json!("high"));
        assert_eq!(glm["thinking"]["type"], json!("enabled"));
        assert!(dummy_llm("zai-org/glm-5.2:thinking").is_glm());
        assert_eq!(
            dummy_llm("zai-org/glm-5.2:thinking").max_tokens(),
            131_072
        );
        let ds_or = Llm {
            api_key: "x".into(),
            url: "https://openrouter.ai/api/v1/chat/completions".into(),
            model: "deepseek/deepseek-v4-pro-0813:thinking".into(),
            client: reqwest::Client::new(),
        }
        .thinking_extra();
        assert_eq!(ds_or["reasoning_effort"], json!("high"));
        assert!(dummy_llm("deepseek/deepseek-v4-pro-0813:thinking").is_deepseek());
        assert!(!dummy_llm("zai-org/glm-5.2:thinking").sees_images());
        assert!(!dummy_llm("deepseek/deepseek-v4-pro-0813:thinking").sees_images());
        assert!(dummy_llm("google/gemini-3.7-flash").sees_images());
        assert!(dummy_llm("gemini-3.7-flash").is_gemini());
        assert!(dummy_llm("gemini-3.7-flash").sees_images());
        assert!(dummy_llm("x-ai/grok-4.6").sees_images());
        assert!(dummy_llm("meta/muse-spark-1.2-contributor").sees_images());
        assert!(dummy_llm("openai/gpt-5.6-luna").sees_images());
        assert!(dummy_llm("openai/gpt-5.6-luna").is_openai());
        let luna = dummy_llm("openai/gpt-5.6-luna");
        assert_eq!(
            luna.thinking_extra_for("coordinatore")["reasoning_effort"],
            json!("medium")
        );
        assert_eq!(
            luna.thinking_extra_for("coder")["reasoning_effort"],
            json!("high")
        );
        assert_eq!(
            dummy_llm("google/gemini-3.7-flash").thinking_extra_for("coordinatore")
                ["reasoning_effort"],
            json!("medium")
        );
        assert_eq!(
            dummy_llm("meta/muse-spark-1.2-contributor").max_tokens(),
            65_536
        );
    }

    #[test]
    fn provider_err_unwraps_google_raw() {
        let payload = json!({
            "error": {
                "message": "Provider returned error",
                "metadata": { "raw": "{\"error\":{\"message\":\"ops.items: missing field.\"}}" }
            }
        });
        let err = provider_err(&payload, reqwest::StatusCode::BAD_REQUEST, "fail");
        assert!(err.contains("ops.items"), "{err}");
    }

    #[test]
    fn timeout_msg_is_recognized() {
        assert!(is_timeout_msg("Timed out waiting for the model (error decoding response body: operation timed out)"));
        assert!(is_timeout_msg("error decoding response body: timed out"));
        assert!(!is_timeout_msg("error decoding response body"));
        assert!(!is_timeout_msg("LLM stream failed"));
    }

    #[test]
    fn coder_wake_cap_blocks_third() {
        let piece = || WorkPiece {
            role: "coder".into(),
            brief: "x".into(),
            text: "Wrote: a".into(),
            from: "coordinatore".into(),
            paths: vec![],
        };
        assert!(coder_wake_cap("coordinatore", "coder", &[]).is_none());
        assert!(coder_wake_cap("coordinatore", "coder", &[piece()]).is_none());
        assert!(coder_wake_cap("coordinatore", "coder", &[piece(), piece()]).is_some());
        assert!(coder_wake_cap("coder", "coder", &[piece()]).is_none());
    }

    #[test]
    fn wrote_line_is_a_built_file() {
        assert!(coder_built("Wrote: index.html\nA page."));
        assert!(!coder_built("Deleted: index.html\nClean."));
        assert!(!coder_built("=== Natale ===\nCena il 24."));
    }

    #[test]
    fn announce_line_does_not_invent_status() {
        assert!(announce_line("", "write a romantic note").is_empty());
        assert!(announce_line(
            "write a romantic note for me please",
            "write a romantic note for me please"
        )
        .is_empty());
        assert_eq!(
            announce_line("A short status from the model.", "write a romantic note"),
            "A short status from the model."
        );
    }

    #[test]
    fn pin_keeps_the_owner_words_as_the_base() {
        let one = pin_owner_order("a site for a bar", "a site for a bar", "a site for a bar");
        assert_eq!(one, "The User asked:\na site for a bar");
        let follow = pin_owner_order(
            "a site for a bar",
            "make the button bigger",
            "make the button bigger",
        );
        assert_eq!(follow, "The User asked:\nmake the button bigger");
        assert!(!follow.contains("a site for a bar"));
        let padded = pin_owner_order(
            "a site for X",
            "a site for X",
            "Build a 5-page restaurant site with booking, a shop, and a cream palette.",
        );
        assert!(padded.starts_with("The User asked:\na site for X\n\n"));
        assert!(padded.contains("cream palette"));
        assert_eq!(
            pin_owner_order("", "", "already a brief"),
            "already a brief"
        );
        let already = "The User asked:\nkeep me";
        assert_eq!(pin_owner_order("x", "x", already), already);
        let old = "The Owner asked:\nkeep me";
        assert_eq!(pin_owner_order("x", "x", old), old);
    }

    #[test]
    fn coder_wrap_look_job_does_not_demand_write() {
        let w = coder_wrap("One page. Contact form is a placeholder. No price in the files.", 1);
        assert!(w.contains("only from the recap"));
        assert!(w.contains("look or report"));
        assert!(!w.contains("That is broken"));
    }

    #[test]
    fn coder_wrap_forbids_invented_numbers() {
        let w = coder_wrap("Wrote: index.html\n\nHero and tel link.", 1);
        assert!(w.contains("Do not add prices"));
        assert!(w.contains("Wrote:"));
    }

    #[test]
    fn correction_flag_is_a_boolean_not_a_phrase() {
        assert!(correction_from_args(r#"{"brief":"fix spacing","correction":true}"#));
        assert!(!correction_from_args(r#"{"brief":"this is a correction"}"#));
        assert!(!correction_from_args("plain brief"));
    }
}

    #[test]
    fn recap_echo_is_stripped() {
        let recap = "index.html — aggiunta in fondo al footer la riga \"Orari\".";
        let out = strip_recap_echo(
            format!("Fatto. Aggiunta la riga. ```\n{}\n```", recap),
            recap,
        );
        assert_eq!(out, "Fatto. Aggiunta la riga.");
        let out = strip_recap_echo(format!("Fatto. {} Aggiunta la riga.", recap), recap);
        assert_eq!(out, "Fatto. Aggiunta la riga.");
        let out = strip_recap_echo(format!("Fatto. `{}` Aggiunta.", recap), recap);
        assert_eq!(out, "Fatto. Aggiunta.");
    }

    #[test]
    fn recap_echo_leaves_real_fence() {
        let recap = "index.html — aggiunta la riga.";
        let out = strip_recap_echo(
            format!("Aggiunta la riga.\n\n```\n{}\n```", recap),
            recap,
        );
        assert_eq!(out, "Aggiunta la riga.");
        // Un fence con contenuto proprio (pezzo da copiare) non si tocca.
        let out = strip_recap_echo("Ecco il testo:\n\n```\nCiao, quando ci vediamo?\n```".into(), recap);
        assert_eq!(out, "Ecco il testo:\n\n```\nCiao, quando ci vediamo?\n```");
    }

    #[test]
    fn paraphrased_recap_fence_is_stripped() {
        let out = strip_recap_echo(
            "In fondo al footer del sito ora c'è la riga \"Tel 333 1234567\".\n\n```\nindex.html — aggiunta in fondo al footer la riga \"Tel 333 1234567\" con link tel:.\n```"
                .into(),
            "index.html — aggiunta in fondo al footer la riga \"Tel 333 1234567\" (con link `tel:`), coerente con le righe già presenti.",
        );
        assert_eq!(
            out,
            "In fondo al footer del sito ora c'è la riga \"Tel 333 1234567\"."
        );
    }

    #[test]
    fn file_line_fence_is_recognized() {
        assert!(is_recap_file_line("index.html — aggiunta la riga."));
        assert!(is_recap_file_line("index.html: aggiunta la riga."));
        assert!(!is_recap_file_line("Ciao, quando ci vediamo?"));
        assert!(!is_recap_file_line("index.html — riga uno\nriga due"));
    }

    #[test]
    fn fatto_prefix_is_stripped() {
        let out = spoken_for_titolare("Fatto. Aggiunta la riga.".into(), &[]);
        assert_eq!(out, "Aggiunta la riga.");
        let out = spoken_for_titolare("Aggiunta la riga.".into(), &[]);
        assert_eq!(out, "Aggiunta la riga.");
    }

    #[test]
    fn spoken_drops_recap_piece() {
        let work = vec![WorkPiece {
            role: "coder".into(),
            brief: "x".into(),
            text: "index.html — aggiunta in fondo al footer la riga \"Orari 08:00-19:00\".".into(),
            from: "coordinatore".into(),
            paths: vec![],
        }];
        let out = spoken_for_titolare(
            "Fatto. Aggiunta in fondo al footer del giardiniere la riga \"Orari 08:00-19:00\". ```\nindex.html — aggiunta in fondo al footer la riga \"Orari 08:00-19:00\".\n```"
                .into(),
            &work,
        );
        assert_eq!(
            out,
            "Aggiunta in fondo al footer del giardiniere la riga \"Orari 08:00-19:00\"."
        );
    }

    #[test]
    fn fence_cleaned_without_work_pieces() {
        let out = spoken_for_titolare(
            "Inserita la riga.\n\n```\nindex.html — aggiunta la riga \"Chiuso il lunedì\".\n```".into(),
            &[],
        );
        assert_eq!(out, "Inserita la riga.");
    }
