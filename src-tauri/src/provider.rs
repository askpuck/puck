//! Chat adapters. Internal messages stay OpenAI-shaped. Only the wire format
//! changes: Gemini native `generateContent`, or the OpenAI-compat path used
//! by NanoGPT / OpenRouter / leftover `/openai` URLs.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{json, Value};

use crate::coordinatore::{
    llm_http_err, message_text, provider_err, tool_calls, Llm,
};
use crate::stream::{apply_delta, finish_message, StreamAcc};

// Local map expires before Gemini's 3600s ttl, so we recreate
// instead of sending a name the API already dropped.
const CACHE_TTL: Duration = Duration::from_secs(45 * 60);
const CACHE_MIN_TOKENS: usize = 4096;
const CACHE_REFRESH: Duration = Duration::from_secs(60);

struct CacheEntry {
    name: String,
    expire: Instant,
}

fn cache_map() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static MAP: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatAdapter {
    OpenAi,
    Gemini,
}

impl Llm {
    pub(crate) fn adapter(&self) -> ChatAdapter {
        if self.uses_native_gemini() {
            ChatAdapter::Gemini
        } else {
            ChatAdapter::OpenAi
        }
    }

    pub(crate) fn uses_native_gemini(&self) -> bool {
        self.is_google() && !self.url.contains("/openai")
    }

    pub(crate) fn gemini_rest_base(&self) -> String {
        let raw = self.url.trim_end_matches('/');
        raw.trim_end_matches("/chat/completions")
            .trim_end_matches("/openai")
            .trim_end_matches('/')
            .to_string()
    }

    pub(crate) fn gemini_model_id(&self) -> String {
        let m = self.model.trim();
        m.strip_prefix("google/")
            .or_else(|| m.strip_prefix("models/"))
            .unwrap_or(m)
            .to_string()
    }
}

pub(crate) fn usage_from_openai(payload: &Value) -> Value {
    payload.get("usage").cloned().unwrap_or(json!({}))
}

pub(crate) fn usage_from_gemini(meta: &Value) -> Value {
    if !meta.is_object() {
        return json!({});
    }
    let prompt = meta.get("promptTokenCount").and_then(Value::as_u64);
    let total = meta.get("totalTokenCount").and_then(Value::as_u64);
    let thoughts = meta.get("thoughtsTokenCount").and_then(Value::as_u64);
    let cached = meta
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let candidates = meta
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": prompt,
        "completion_tokens": candidates,
        "total_tokens": total,
        "thoughts_tokens": thoughts,
        "cached_tokens": cached,
        "prompt_tokens_details": { "cached_tokens": cached },
    })
}

pub(crate) fn log_usage(usage: &Value) -> Value {
    if !usage.is_object() {
        return json!({});
    }
    let mut out = json!({});
    for key in [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "cached_tokens",
        "thoughts_tokens",
    ] {
        if let Some(v) = usage.get(key) {
            out[key] = v.clone();
        }
    }
    if let Some(c) = usage.pointer("/prompt_tokens_details/cached_tokens") {
        out["cached_tokens"] = c.clone();
    }
    out
}

fn estimate_tokens(s: &str) -> usize {
    (s.len() / 4).max(1)
}

fn hash_key(parts: &[&str]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn explicit_cache_key(
    llm: &Llm,
    system: &str,
    tools: Option<&[Value]>,
    tool_choice: &str,
) -> String {
    let tools_json = tools
        .map(|t| openai_tools_to_gemini(t).to_string())
        .unwrap_or_default();
    hash_key(&[
        &llm.gemini_model_id(),
        system,
        &tools_json,
        tool_choice,
    ])
}

fn forget_explicit_cache(key: &str) {
    if let Ok(mut map) = cache_map().lock() {
        map.remove(key);
    }
}

fn cache_gone(status: reqwest::StatusCode, payload: &Value) -> bool {
    if status.as_u16() != 403 && status.as_u16() != 404 {
        return false;
    }
    let msg = payload
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let blob = format!("{payload} {msg}").to_ascii_lowercase();
    blob.contains("cachedcontent") || blob.contains("cached content")
}

fn clean_schema(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if k == "additionalProperties" || k == "$schema" || k == "$id" {
                    continue;
                }
                out.insert(k.clone(), clean_schema(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(clean_schema).collect()),
        other => other.clone(),
    }
}

pub(crate) fn openai_tools_to_gemini(tools: &[Value]) -> Value {
    let decls: Vec<Value> = tools
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            let name = f.get("name").and_then(Value::as_str)?.to_string();
            if name.is_empty() {
                return None;
            }
            let mut decl = json!({ "name": name });
            if let Some(d) = f.get("description").and_then(Value::as_str) {
                decl["description"] = json!(d);
            }
            if let Some(params) = f.get("parameters") {
                decl["parameters"] = clean_schema(params);
            }
            Some(decl)
        })
        .collect();
    json!([{ "functionDeclarations": decls }])
}

fn tool_choice_gemini(choice: &str) -> Option<Value> {
    match choice.trim() {
        "" | "auto" => Some(json!({ "functionCallingConfig": { "mode": "AUTO" } })),
        "none" => Some(json!({ "functionCallingConfig": { "mode": "NONE" } })),
        "required" | "any" => Some(json!({ "functionCallingConfig": { "mode": "ANY" } })),
        name if !name.is_empty() => Some(json!({
            "functionCallingConfig": {
                "mode": "ANY",
                "allowedFunctionNames": [name]
            }
        })),
        _ => None,
    }
}

fn thinking_level_for(model: &str, want: &str) -> String {
    let m = model.to_ascii_lowercase();
    let allows_minimal = m.contains("lite")
        || m.contains("3.5")
        || m.contains("3.6")
        || m.contains("3-flash")
        || m.contains("2.5");
    match want.to_ascii_lowercase().as_str() {
        "none" | "minimal" if allows_minimal => "MINIMAL".into(),
        "none" | "low" => "LOW".into(),
        "medium" => "MEDIUM".into(),
        _ => "HIGH".into(),
    }
}

fn extra_disables_thinking(extra: Option<&Value>) -> bool {
    extra
        .and_then(|v| v.pointer("/thinking/type"))
        .and_then(Value::as_str)
        == Some("disabled")
}

fn extra_thinking_level(extra: Option<&Value>) -> Option<String> {
    extra
        .and_then(|v| {
            v.pointer("/extra_body/google/thinking_config/thinking_level")
                .or_else(|| v.pointer("/generationConfig/thinkingConfig/thinkingLevel"))
                .or_else(|| v.get("reasoning_effort"))
        })
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

pub(crate) fn gemini_thinking_config(llm: &Llm, extra: Option<&Value>, role: &str) -> Value {
    if extra_disables_thinking(extra) {
        return json!({
            "includeThoughts": false,
            "thinkingLevel": thinking_level_for(&llm.model, "low")
        });
    }
    let want = extra_thinking_level(extra)
        .unwrap_or_else(|| crate::coordinatore::role_effort(role).to_string());
    json!({
        "includeThoughts": true,
        "thinkingLevel": thinking_level_for(&llm.model, &want)
    })
}

fn content_parts(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![json!({ "text": s })]
            }
        }
        Value::Array(arr) => arr.iter().filter_map(openai_part_to_gemini).collect(),
        _ => Vec::new(),
    }
}

fn openai_part_to_gemini(part: &Value) -> Option<Value> {
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        if part.get("type").and_then(Value::as_str) == Some("image_url") {
            return data_url_part(part);
        }
        if !text.is_empty() {
            return Some(json!({ "text": text }));
        }
    }
    if part.get("type").and_then(Value::as_str) == Some("image_url") {
        return data_url_part(part);
    }
    if let Some(s) = part.as_str() {
        if !s.is_empty() {
            return Some(json!({ "text": s }));
        }
    }
    None
}

fn data_url_part(part: &Value) -> Option<Value> {
    let url = part
        .pointer("/image_url/url")
        .and_then(Value::as_str)
        .or_else(|| part.get("url").and_then(Value::as_str))?;
    let rest = url.strip_prefix("data:")?;
    let (meta, b64) = rest.split_once(";base64,")?;
    Some(json!({
        "inlineData": {
            "mimeType": meta,
            "data": b64
        }
    }))
}

fn function_call_part(call: &Value, idx: usize) -> Value {
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let args_raw = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let args: Value = serde_json::from_str(args_raw).unwrap_or(json!({}));
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("call-{idx}"));
    let mut fc = json!({ "name": name, "args": args });
    fc["id"] = json!(id);
    let mut part = json!({ "functionCall": fc });
    if let Some(sig) = call
        .get("thought_signature")
        .or_else(|| call.get("thoughtSignature"))
    {
        if !sig.is_null() {
            part["thoughtSignature"] = sig.clone();
        }
    }
    part
}

fn assistant_parts(msg: &Value) -> Vec<Value> {
    if let Some(parts) = msg.get("gemini_parts").and_then(Value::as_array) {
        if !parts.is_empty() {
            return parts.clone();
        }
    }
    let mut parts = content_parts(msg.get("content").unwrap_or(&Value::Null));
    for (i, call) in tool_calls(msg).iter().enumerate() {
        parts.push(function_call_part(call, i));
    }
    if parts.is_empty() {
        if let Some(thought) = msg
            .get("reasoning_content")
            .or_else(|| msg.get("reasoning"))
            .and_then(Value::as_str)
        {
            if !thought.is_empty() {
                parts.push(json!({ "text": thought, "thought": true }));
            }
        }
    }
    parts
}

fn function_response_part(msg: &Value, fallback_name: &str) -> Value {
    let id = msg
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = msg
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_name);
    let text = message_text(msg);
    let mut fr = json!({
        "name": name,
        "response": { "result": text }
    });
    if !id.is_empty() {
        fr["id"] = json!(id);
    }
    json!({ "functionResponse": fr })
}

fn name_for_tool_id(history: &[Value], id: &str) -> String {
    if id.is_empty() {
        return "tool".into();
    }
    for msg in history.iter().rev() {
        for call in tool_calls(msg) {
            if call.get("id").and_then(Value::as_str) == Some(id) {
                if let Some(n) = call.pointer("/function/name").and_then(Value::as_str) {
                    return n.to_string();
                }
            }
        }
    }
    "tool".into()
}

/// Split OpenAI messages into Gemini `systemInstruction` + `contents`.
pub(crate) fn openai_messages_to_gemini(messages: &[Value]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut pending_fn: Vec<Value> = Vec::new();

    let flush_fn = |contents: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if pending.is_empty() {
            return;
        }
        contents.push(json!({
            "role": "user",
            "parts": std::mem::take(pending)
        }));
    };

    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" => {
                let text = message_text(msg);
                if text.is_empty() {
                    continue;
                }
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&text);
            }
            "tool" => {
                let id = msg
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                pending_fn.push(function_response_part(msg, &name_for_tool_id(messages, id)));
            }
            "assistant" => {
                flush_fn(&mut contents, &mut pending_fn);
                let parts = assistant_parts(msg);
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            _ => {
                flush_fn(&mut contents, &mut pending_fn);
                let parts = content_parts(msg.get("content").unwrap_or(&Value::Null));
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }
    flush_fn(&mut contents, &mut pending_fn);

    if contents
        .first()
        .and_then(|c| c.get("role"))
        .and_then(Value::as_str)
        == Some("model")
    {
        contents.insert(
            0,
            json!({ "role": "user", "parts": [{ "text": "(continue)" }] }),
        );
    }
    (system, contents)
}

fn gemini_args_string(args: &Value) -> String {
    match args {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn gemini_parts_to_openai(parts: &[Value]) -> Value {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let thought = part.get("thought").and_then(Value::as_bool).unwrap_or(false);
        if let Some(fc) = part.get("functionCall") {
            let name = fc.get("name").and_then(Value::as_str).unwrap_or("");
            let id = fc
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("call-{i}"));
            let args = fc.get("args").unwrap_or(&Value::Null);
            let mut call = json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": gemini_args_string(args)
                }
            });
            if let Some(sig) = part.get("thoughtSignature") {
                call["thought_signature"] = sig.clone();
            }
            tool_calls.push(call);
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if thought {
                reasoning.push_str(text);
            } else {
                content.push_str(text);
            }
        }
    }
    let mut msg = json!({
        "role": "assistant",
        "content": content,
    });
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning);
        msg["reasoning"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        msg["tool_calls"] = json!(tool_calls);
    }
    if !parts.is_empty() {
        msg["gemini_parts"] = json!(parts);
    }
    msg
}

pub(crate) fn build_gemini_body(
    llm: &Llm,
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &str,
    extra: Option<&Value>,
    role: &str,
    cached: Option<&str>,
) -> Value {
    let (system, contents) = openai_messages_to_gemini(messages);
    let mut body = json!({ "contents": contents });
    if let Some(name) = cached {
        body["cachedContent"] = json!(name);
    } else if !system.is_empty() {
        body["systemInstruction"] = json!({
            "parts": [{ "text": system }]
        });
    }
    if cached.is_none() {
        if let Some(tools) = tools {
            if !tools.is_empty() {
                body["tools"] = openai_tools_to_gemini(tools);
            }
        }
        if tools.is_some() {
            if let Some(cfg) = tool_choice_gemini(tool_choice) {
                body["toolConfig"] = cfg;
            }
        }
    }
    let mut gen = json!({
        "maxOutputTokens": llm.max_tokens(),
        "thinkingConfig": gemini_thinking_config(llm, extra, role)
    });
    if let Some(temp) = extra.and_then(|v| v.get("temperature")) {
        gen["temperature"] = temp.clone();
    }
    body["generationConfig"] = gen;
    body
}

fn cache_payload(
    llm: &Llm,
    system: &str,
    tools: Option<&[Value]>,
    tool_choice: &str,
) -> Option<Value> {
    if system.is_empty() {
        return None;
    }
    let tools_json = tools
        .map(|t| openai_tools_to_gemini(t).to_string())
        .unwrap_or_default();
    if estimate_tokens(system) + estimate_tokens(&tools_json) < CACHE_MIN_TOKENS {
        return None;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut body = json!({
        "model": format!("models/{}", llm.gemini_model_id()),
        "systemInstruction": { "parts": [{ "text": system }] },
        "ttl": "3600s",
        "displayName": format!(
            "puck-{}-{stamp}",
            hash_key(&[system, &tools_json, tool_choice])
        )
    });
    if let Some(tools) = tools {
        if !tools.is_empty() {
            body["tools"] = openai_tools_to_gemini(tools);
        }
    }
    if tools.is_some() {
        if let Some(cfg) = tool_choice_gemini(tool_choice) {
            body["toolConfig"] = cfg;
        }
    }
    Some(body)
}

async fn ensure_explicit_cache(
    llm: &Llm,
    system: &str,
    tools: Option<&[Value]>,
    tool_choice: &str,
) -> Option<String> {
    let payload = cache_payload(llm, system, tools, tool_choice)?;
    let key = explicit_cache_key(llm, system, tools, tool_choice);
    if let Ok(map) = cache_map().lock() {
        if let Some(entry) = map.get(&key) {
            if entry.expire.saturating_duration_since(Instant::now()) > CACHE_REFRESH {
                return Some(entry.name.clone());
            }
        }
    }
    let url = format!("{}/cachedContents", llm.gemini_rest_base());
    let response = llm
        .bind(llm.client.post(&url))
        .json(&payload)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    let name = body.get("name").and_then(Value::as_str)?.to_string();
    if let Ok(mut map) = cache_map().lock() {
        map.insert(
            key,
            CacheEntry {
                name: name.clone(),
                expire: Instant::now() + CACHE_TTL,
            },
        );
    }
    Some(name)
}

fn gemini_url(llm: &Llm, stream: bool) -> String {
    let method = if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    };
    let mut url = format!(
        "{}/models/{}:{}",
        llm.gemini_rest_base(),
        llm.gemini_model_id(),
        method
    );
    if stream {
        url.push_str("?alt=sse");
    }
    url
}

pub(crate) async fn gemini_complete(
    llm: &Llm,
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &str,
    extra: Option<&Value>,
    role: &str,
) -> Result<(Value, Value), String> {
    let (system, _) = openai_messages_to_gemini(messages);
    let key = explicit_cache_key(llm, &system, tools, tool_choice);
    let mut cached = ensure_explicit_cache(llm, &system, tools, tool_choice).await;
    let payload = loop {
        let body = build_gemini_body(
            llm,
            messages,
            tools,
            tool_choice,
            extra,
            role,
            cached.as_deref(),
        );
        let response = llm
            .bind(llm.client.post(gemini_url(llm, false)))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Network: {}", llm_http_err(e)))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|e| format!("Bad JSON from Gemini: {}", llm_http_err(e)))?;
        if status.is_success() {
            break payload;
        }
        if cached.is_some() && cache_gone(status, &payload) {
            forget_explicit_cache(&key);
            cached = None;
            continue;
        }
        return Err(provider_err(&payload, status, "Gemini request failed"));
    };
    let parts = payload
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if parts.is_empty()
        && payload
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
            .is_none()
    {
        return Err("Gemini returned no message.".into());
    }
    let mut message = gemini_parts_to_openai(&parts);
    let usage = usage_from_gemini(payload.get("usageMetadata").unwrap_or(&json!({})));
    message["usage"] = usage.clone();
    Ok((message, usage))
}

pub(crate) fn apply_gemini_chunk(acc: &mut StreamAcc, chunk: &Value) {
    if let Some(meta) = chunk.get("usageMetadata") {
        acc.usage = Some(usage_from_gemini(meta));
    }
    let Some(parts) = chunk
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    else {
        return;
    };
    for part in parts {
        acc.gemini_parts.push(part.clone());
        let thought = part.get("thought").and_then(Value::as_bool).unwrap_or(false);
        if let Some(fc) = part.get("functionCall") {
            let idx = acc.tools.len();
            let name = fc.get("name").and_then(Value::as_str).unwrap_or("");
            let id = fc
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("call-{idx}"));
            let args = gemini_args_string(fc.get("args").unwrap_or(&json!({})));
            acc.tools
                .entry(idx)
                .or_insert_with(|| (id, name.to_string(), args));
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if thought {
                apply_delta(acc, &json!({ "reasoning": text }));
            } else {
                apply_delta(acc, &json!({ "content": text }));
            }
        }
    }
}

pub(crate) async fn gemini_stream(
    llm: &Llm,
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &str,
    extra: Option<&Value>,
    role: &str,
    mut on_chunk: impl FnMut(&StreamAcc),
) -> Result<Value, String> {
    let (system, _) = openai_messages_to_gemini(messages);
    let key = explicit_cache_key(llm, &system, tools, tool_choice);
    let mut cached = ensure_explicit_cache(llm, &system, tools, tool_choice).await;
    let response = loop {
        let body = build_gemini_body(
            llm,
            messages,
            tools,
            tool_choice,
            extra,
            role,
            cached.as_deref(),
        );
        let response = llm
            .bind(llm.client.post(gemini_url(llm, true)))
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Network: {}", llm_http_err(e)))?;
        let status = response.status();
        if status.is_success() {
            break response;
        }
        let payload: Value = response.json().await.unwrap_or(json!({}));
        if cached.is_some() && cache_gone(status, &payload) {
            forget_explicit_cache(&key);
            cached = None;
            continue;
        }
        return Err(provider_err(&payload, status, "Gemini stream failed"));
    };
    let mut acc = StreamAcc::default();
    let mut buf = String::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| llm_http_err(e))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buf.find('\n') {
            let mut line = buf[..pos].to_string();
            buf.drain(..=pos);
            if line.ends_with('\r') {
                line.pop();
            }
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
                continue;
            }
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            apply_gemini_chunk(&mut acc, &v);
            on_chunk(&acc);
        }
    }
    if acc.content.is_empty() && acc.tools.is_empty() && acc.reasoning.is_empty() {
        return Err("Empty stream.".into());
    }
    let mut message = finish_message(&acc);
    if !acc.gemini_parts.is_empty() {
        message["gemini_parts"] = json!(acc.gemini_parts);
    }
    if let Some(usage) = acc.usage {
        message["usage"] = usage;
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamAcc;

    fn dummy_google() -> Llm {
        Llm {
            api_key: "x".into(),
            url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "gemini-3.7-flash".into(),
            client: reqwest::Client::new(),
        }
    }

    #[test]
    fn native_gemini_is_not_openai_compat() {
        let llm = dummy_google();
        assert!(llm.is_google());
        assert!(llm.uses_native_gemini());
        assert_eq!(llm.adapter(), ChatAdapter::Gemini);
        assert_eq!(
            llm.gemini_rest_base(),
            "https://generativelanguage.googleapis.com/v1beta"
        );
        let compat = Llm {
            api_key: "x".into(),
            url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
                .into(),
            model: "gemini-3.7-flash".into(),
            client: reqwest::Client::new(),
        };
        assert!(compat.is_google());
        assert!(!compat.uses_native_gemini());
        assert_eq!(compat.adapter(), ChatAdapter::OpenAi);
    }

    #[test]
    fn system_stays_in_instruction_conversation_in_contents() {
        let (sys, contents) = openai_messages_to_gemini(&[
            json!({"role":"system","content":"Standing orders."}),
            json!({"role":"user","content":"ciao"}),
            json!({"role":"assistant","content":"ok","tool_calls":[{
                "id":"c1","type":"function",
                "function":{"name":"run","arguments":"{\"command\":\"ls\"}"}
            }]}),
            json!({"role":"tool","tool_call_id":"c1","content":"exit: 0"}),
        ]);
        assert_eq!(sys, "Standing orders.");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert!(contents[1]["parts"][1]["functionCall"]["name"] == "run");
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "run"
        );
    }

    #[test]
    fn thought_signatures_round_trip() {
        let parts = vec![
            json!({"text":"planning","thought":true,"thoughtSignature":"SIG1"}),
            json!({"functionCall":{"id":"c1","name":"read_file","args":{"path":"a.txt"}},"thoughtSignature":"SIG2"}),
        ];
        let msg = gemini_parts_to_openai(&parts);
        assert_eq!(msg["reasoning_content"], "planning");
        assert_eq!(msg["tool_calls"][0]["thought_signature"], "SIG2");
        assert_eq!(msg["gemini_parts"], json!(parts));
        let (_, contents) = openai_messages_to_gemini(&[msg]);
        let model = contents
            .iter()
            .find(|c| c.get("role").and_then(Value::as_str) == Some("model"))
            .expect("model turn");
        assert_eq!(model["parts"], json!(parts));
    }

    #[test]
    fn image_data_url_becomes_inline_data() {
        let (_, contents) = openai_messages_to_gemini(&[json!({
            "role":"user",
            "content":[
                {"type":"text","text":"look"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}}
            ]
        })]);
        assert_eq!(contents[0]["parts"][0]["text"], "look");
        assert_eq!(
            contents[0]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(contents[0]["parts"][1]["inlineData"]["data"], "QUJD");
    }

    #[test]
    fn tools_lose_openai_wrapper_and_additional_properties() {
        let gem = openai_tools_to_gemini(&[json!({
            "type":"function",
            "cache_control":{"type":"ephemeral"},
            "function":{
                "name":"run",
                "description":"Run a command.",
                "parameters":{
                    "type":"object",
                    "additionalProperties": false,
                    "properties":{"command":{"type":"string"}}
                }
            }
        })]);
        assert_eq!(gem[0]["functionDeclarations"][0]["name"], "run");
        assert!(gem[0]["functionDeclarations"][0]["parameters"]
            .get("additionalProperties")
            .is_none());
    }

    #[test]
    fn cached_prefix_is_omitted_from_request_body() {
        let llm = dummy_google();
        let body = build_gemini_body(
            &llm,
            &[
                json!({"role":"system","content":"orders"}),
                json!({"role":"user","content":"hi"}),
            ],
            Some(&[json!({"type":"function","function":{"name":"run","parameters":{"type":"object"}}})]),
            "auto",
            None,
            "coder",
            Some("cachedContents/abc"),
        );
        assert_eq!(body["cachedContent"], "cachedContents/abc");
        assert!(body.get("systemInstruction").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("toolConfig").is_none());
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
    }

    #[test]
    fn lite_allows_minimal_thinking() {
        let lite = Llm {
            api_key: "x".into(),
            url: "https://generativelanguage.googleapis.com/v1beta".into(),
            model: "gemini-3.5-flash-lite".into(),
            client: reqwest::Client::new(),
        };
        let cfg = gemini_thinking_config(
            &lite,
            Some(&json!({"reasoning_effort":"none"})),
            "coordinatore",
        );
        assert_eq!(cfg["thinkingLevel"], "MINIMAL");
        let flash = dummy_google();
        let cfg = gemini_thinking_config(
            &flash,
            Some(&json!({"reasoning_effort":"none"})),
            "coordinatore",
        );
        assert_eq!(cfg["thinkingLevel"], "LOW");
    }

    #[test]
    fn usage_maps_cached_tokens() {
        let u = usage_from_gemini(&json!({
            "promptTokenCount": 18000,
            "candidatesTokenCount": 20,
            "totalTokenCount": 18025,
            "cachedContentTokenCount": 12000,
            "thoughtsTokenCount": 5
        }));
        assert_eq!(u["cached_tokens"], 12000);
        assert_eq!(u["prompt_tokens_details"]["cached_tokens"], 12000);
        assert_eq!(log_usage(&u)["cached_tokens"], 12000);
    }

    #[test]
    fn stream_chunk_keeps_function_call_and_thought() {
        let mut acc = StreamAcc::default();
        apply_gemini_chunk(
            &mut acc,
            &json!({
                "candidates":[{
                    "content":{"parts":[
                        {"text":"plan","thought":true},
                        {"functionCall":{"name":"run","args":{"command":"open x"}}}
                    ]}
                }],
                "usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":8}
            }),
        );
        assert_eq!(acc.reasoning, "plan");
        assert_eq!(acc.tools.get(&0).unwrap().1, "run");
        assert_eq!(acc.usage.as_ref().unwrap()["cached_tokens"], 8);
        assert_eq!(acc.gemini_parts.len(), 2);
    }

    #[test]
    fn cache_payload_skips_short_prefix() {
        let llm = dummy_google();
        assert!(cache_payload(&llm, "hi", None, "auto").is_none());
        let long = "word ".repeat(5000);
        let payload = cache_payload(
            &llm,
            &long,
            Some(&[json!({"type":"function","function":{"name":"run","parameters":{"type":"object"}}})]),
            "auto",
        )
        .expect("long prefix");
        assert!(payload.get("systemInstruction").is_some());
        assert!(payload.get("tools").is_some());
        assert!(payload.get("toolConfig").is_some());
        let name = payload["displayName"].as_str().unwrap();
        assert!(name.starts_with("puck-"));
        assert!(name.matches('-').count() >= 2);
    }

    #[test]
    fn cache_gone_is_only_missing_cached_content() {
        let gone = reqwest::StatusCode::FORBIDDEN;
        assert!(cache_gone(
            gone,
            &json!({"error":{"message":"CachedContent not found (or permission denied)"}})
        ));
        assert!(cache_gone(
            reqwest::StatusCode::NOT_FOUND,
            &json!({"error":{"message":"Requested entity was not found. cachedContent"}})
        ));
        assert!(!cache_gone(
            reqwest::StatusCode::BAD_REQUEST,
            &json!({"error":{"message":"CachedContent can not be used with toolConfig"}})
        ));
        assert!(!cache_gone(
            gone,
            &json!({"error":{"message":"Permission denied on this API key"}})
        ));
    }

    #[test]
    fn forget_drops_only_that_key() {
        {
            let mut map = cache_map().lock().unwrap();
            map.insert(
                "keep".into(),
                CacheEntry {
                    name: "cachedContents/a".into(),
                    expire: Instant::now() + CACHE_TTL,
                },
            );
            map.insert(
                "drop".into(),
                CacheEntry {
                    name: "cachedContents/b".into(),
                    expire: Instant::now() + CACHE_TTL,
                },
            );
        }
        forget_explicit_cache("drop");
        let map = cache_map().lock().unwrap();
        assert!(map.contains_key("keep"));
        assert!(!map.contains_key("drop"));
    }
}
