use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::coordinatore::{
    complete_with, is_timeout_msg, llm_http_err, message_text, provider_err, tool_calls, Llm,
};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThinkPulse {
    kind: &'static str,
    role: String,
    text: String,
    brief: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

pub(crate) fn emit_think(app: &AppHandle, role: &str, text: &str, done: bool) {
    emit_stream(app, "think", role, text, done, None, None);
}

fn emit_speak(app: &AppHandle, role: &str, text: &str, done: bool) {
    emit_stream(app, "speak", role, text, done, None, None);
}

fn emit_brief(app: &AppHandle, to: &str, from: &str, text: &str, done: bool) {
    emit_stream(app, "brief", to, text, done, Some(from), None);
}

fn emit_trace_live(app: &AppHandle, role: &str, text: &str, done: bool) {
    emit_stream(app, "trace", role, text, done, None, None);
}

fn emit_wrote_live(app: &AppHandle, role: &str, path: &str, text: &str, done: bool) {
    emit_stream(app, "wrote", role, text, done, None, Some(path));
}

fn emit_stream(
    app: &AppHandle,
    kind: &'static str,
    role: &str,
    text: &str,
    done: bool,
    from: Option<&str>,
    path: Option<&str>,
) {
    let _ = app.emit(
        "puck-crew",
        ThinkPulse {
            kind,
            role: role.to_string(),
            text: text.to_string(),
            brief: if done { "done" } else { "live" }.into(),
            from: from.map(ToOwned::to_owned),
            path: path.map(ToOwned::to_owned),
        },
    );
}


pub(crate) fn peek_brief_arg(args: &str) -> String {
    peek_json_field(args, "brief")
}

pub(crate) fn peek_json_field(args: &str, key: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(args) {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return s.to_string();
            }
        }
    }
    let needle = format!("\"{key}\"");
    let Some(pos) = args.find(&needle) else {
        return String::new();
    };
    let rest = &args[pos + needle.len()..];
    let Some(colon) = rest.find(':') else {
        return String::new();
    };
    let s = rest[colon + 1..].trim_start();
    let Some(body) = s.strip_prefix('"') else {
        return String::new();
    };
    take_json_string(body)
}

fn take_json_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() == 4 {
                        if let Ok(v) = u32::from_str_radix(&hex, 16) {
                            if let Some(ch) = char::from_u32(v) {
                                out.push(ch);
                            }
                        }
                    }
                }
                Some(other) => out.push(other),
                None => break,
            }
        } else if c == '"' {
            break;
        } else {
            out.push(c);
        }
    }
    out
}

fn emit_live_briefs(app: &AppHandle, from: &str, acc: &StreamAcc, done: bool) -> bool {
    let mut slots: Vec<(usize, &String, &String)> = acc
        .tools
        .iter()
        .map(|(i, (_id, name, args))| (*i, name, args))
        .collect();
    slots.sort_by_key(|(i, _, _)| *i);
    let mut any = false;
    for (_, name, args) in slots {
        let to = match name.as_str() { "ask_coder" => "coder", _ => continue };
        let text = peek_brief_arg(args);
        if text.is_empty() {
            continue;
        }
        emit_brief(app, to, from, &text, done);
        any = true;
    }
    any
}

pub(crate) fn thought_should_hide(_acc: &StreamAcc, stream_done: bool, _reason_grew: bool) -> bool {
    stream_done
}

fn think_text(acc: &StreamAcc) -> &str {
    &acc.reasoning
}

pub(crate) fn is_reasoning_echo(content: &str, reasoning: &str) -> bool {
    let content = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let reasoning = reasoning.split_whitespace().collect::<Vec<_>>().join(" ");
    !content.is_empty() && !reasoning.is_empty() && content == reasoning
}

fn should_speak(_role: &str, acc: &StreamAcc) -> bool {
    if acc.content.is_empty() {
        return false;
    }
    !is_reasoning_echo(&acc.content, &acc.reasoning)
}

fn clip_one_line(s: &str, max: usize) -> String {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max {
        return t;
    }
    format!("{}…", t.chars().take(max).collect::<String>())
}

fn peek_json_strings(args: &str, keys: &[&str]) -> Vec<String> {
    if let Ok(v) = serde_json::from_str::<Value>(args) {
        let mut out = Vec::new();
        for key in keys {
            match v.get(*key) {
                Some(Value::String(s)) => {
                    let t = s.trim();
                    if !t.is_empty() {
                        out.push(t.to_string());
                    }
                }
                Some(Value::Array(arr)) => {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            let t = s.trim();
                            if !t.is_empty() {
                                out.push(t.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    for key in keys {
        let s = peek_json_field(args, key);
        if !s.is_empty() {
            return vec![s];
        }
    }
    Vec::new()
}

fn tool_live_label(name: &str, args: &str) -> Option<String> {
    let path = peek_json_field(args, "path");
    match name {
        "write_file" | "patch_file" | "todo_write" => None,
        "ask_coder" | "ask_user" => None,
        "read_file" if !path.is_empty() => Some(format!("Read {path}")),
        "list_dir" => Some(format!(
            "Listed {}",
            if path.is_empty() { "." } else { path.as_str() }
        )),
        "glob" => {
            let pat = peek_json_field(args, "pattern");
            if pat.is_empty() {
                Some("Looked for files".into())
            } else {
                Some(format!("Looked for {pat}"))
            }
        }
        "search" => {
            let q = peek_json_field(args, "query");
            if q.is_empty() {
                Some("Searched files".into())
            } else {
                Some(format!("Searched {q}"))
            }
        }
        "run" => {
            let cmd = peek_json_field(args, "command");
            if cmd.is_empty() {
                Some("Ran a command".into())
            } else {
                Some(format!("Ran {}", clip_one_line(&cmd, 48)))
            }
        }
        "view_page" if !path.is_empty() => Some(format!("Opened {path}")),
        "view_image" if !path.is_empty() => Some(format!("Saw {path}")),
        "delete_file" if !path.is_empty() => Some(format!("Deleted {path}")),
        "move_file" => {
            let from = peek_json_field(args, "from");
            let to = peek_json_field(args, "to");
            if from.is_empty() && to.is_empty() {
                None
            } else {
                Some(format!("Moved {from} → {to}"))
            }
        }
        "create_image" if !path.is_empty() => Some(format!("Creating {path}")),
        "search_web" => {
            let qs = peek_json_strings(args, &["queries", "query"]);
            match qs.as_slice() {
                [] => Some("Searched the web".into()),
                [one] => Some(format!("Searched: {}", clip_one_line(one, 48))),
                [first, rest @ ..] => Some(format!(
                    "Searched: {} (+{})",
                    clip_one_line(first, 32),
                    rest.len()
                )),
            }
        }
        "read_page" => {
            let urls = peek_json_strings(args, &["urls", "url"]);
            match urls.as_slice() {
                [] => Some("Read a page".into()),
                [one] => Some(format!("Read: {}", clip_one_line(one, 48))),
                [first, rest @ ..] => Some(format!(
                    "Read: {} (+{})",
                    clip_one_line(first, 32),
                    rest.len()
                )),
            }
        }
        "patch_comune" => Some("Updated owner memory".into()),
        "patch_project" => Some("Updated project memory".into()),
        other if !other.is_empty() => Some(format!("Used {other}")),
        _ => None,
    }
}

fn emit_live_work(app: &AppHandle, role: &str, acc: &StreamAcc, done: bool) -> bool {
    let mut slots: Vec<(usize, &String, &String)> = acc
        .tools
        .iter()
        .map(|(i, (_id, name, args))| (*i, name, args))
        .collect();
    slots.sort_by_key(|(i, _, _)| *i);
    let mut any = false;
    for (_, name, args) in slots {
        if name == "write_file" || name == "patch_file" {
            let path = peek_json_field(args, "path");
            if path.is_empty() {
                continue;
            }
            let body = if name.as_str() == "write_file" {
                peek_json_field(args, "content")
            } else {
                let search = peek_json_field(args, "search");
                let replace = peek_json_field(args, "replace");
                if search.is_empty() && replace.is_empty() {
                    String::new()
                } else {
                    format!("- {search}\n+ {replace}")
                }
            };
            emit_wrote_live(app, role, &path, &body, false);
            any = true;
            continue;
        }
        if let Some(label) = tool_live_label(name, args) {
            emit_trace_live(app, role, &label, done);
            any = true;
        }
    }
    any
}

fn emit_live_output(
    app: &AppHandle,
    role: &str,
    acc: &StreamAcc,
    done: bool,
    reason_grew: bool,
) -> bool {
    let mut any = false;
    let thought = think_text(acc);
    // Il thinking in diretta si riemette solo quando cresce (o alla fine):
    // i provider che rispediscono lo stesso reasoning a ogni delta non devono
    // produrre una sfilza di thought identici.
    let think_now = done || reason_grew || thought.is_empty();
    if think_now {
        emit_think(
            app,
            role,
            thought,
            thought_should_hide(acc, done, reason_grew),
        );
        any = true;
    }
    // Il Coordinatore non parla a metà: i pezzi di contenuto in streaming
    // arrivano solo col finale (il filtro recap gira lì). Il Coder continua
    // a streammare nella sua chat.
    let speak_now = done || role != "coordinatore";
    if speak_now && should_speak(role, acc) {
        emit_speak(app, role, &acc.content, done);
        any = true;
    }
    if emit_live_briefs(app, role, acc, done) {
        any = true;
    }
    if emit_live_work(app, role, acc, done) {
        any = true;
    }
    any
}

#[derive(Default)]
pub(crate) struct StreamAcc {
    pub content: String,
    pub reasoning: String,
    pub tools: HashMap<usize, (String, String, String)>,
    pub gemini_parts: Vec<Value>,
    pub usage: Option<Value>,
    in_google_thought: bool,
}

fn push_reason(acc: &mut StreamAcc, s: &str) {
    if s.is_empty() {
        return;
    }
    if acc.reasoning.is_empty() {
        acc.reasoning.push_str(s);
        return;
    }
    if s == acc.reasoning || acc.reasoning.ends_with(s) {
        return;
    }
    if s.starts_with(&acc.reasoning) {
        acc.reasoning = s.to_string();
        return;
    }
    acc.reasoning.push_str(s);
}

fn echo_of_content(acc: &StreamAcc, s: &str) -> bool {
    let got = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let have = acc.content.split_whitespace().collect::<Vec<_>>().join(" ");
    !got.is_empty() && !have.is_empty() && (got == have || have.ends_with(&got) || got.ends_with(&have))
}

fn lift_google_thoughts(acc: &mut StreamAcc) {
    const OPEN: &str = "<thought>";
    const CLOSE: &str = "</thought>";
    loop {
        if let Some(start) = acc.content.find(OPEN) {
            let after = start + OPEN.len();
            if let Some(rel) = acc.content[after..].find(CLOSE) {
                let thought = acc.content[after..after + rel].to_string();
                let rest = acc.content[after + rel + CLOSE.len()..].to_string();
                acc.content.truncate(start);
                acc.content.push_str(&rest);
                push_reason(acc, &thought);
                acc.in_google_thought = false;
                continue;
            }
            let thought = acc.content[after..].to_string();
            acc.content.truncate(start);
            push_reason(acc, &thought);
            acc.in_google_thought = true;
            break;
        }
        if acc.in_google_thought {
            if let Some(end) = acc.content.find(CLOSE) {
                let thought = acc.content[..end].to_string();
                let rest = acc.content[end + CLOSE.len()..].to_string();
                acc.content = rest;
                push_reason(acc, &thought);
                acc.in_google_thought = false;
                continue;
            }
            let thought = std::mem::take(&mut acc.content);
            push_reason(acc, &thought);
        }
        break;
    }
}

pub(crate) fn apply_delta(acc: &mut StreamAcc, delta: &Value) {
    if let Some(s) = delta.get("content").and_then(Value::as_str) {
        acc.content.push_str(s);
        lift_google_thoughts(acc);
    }
    let r = delta.get("reasoning").and_then(Value::as_str).unwrap_or("");
    let rc = delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !r.is_empty() && r == rc {
        if !echo_of_content(acc, r) {
            push_reason(acc, r);
        }
    } else {
        if !echo_of_content(acc, r) {
            push_reason(acc, r);
        }
        if !echo_of_content(acc, rc) {
            push_reason(acc, rc);
        }
    }
    for key in ["thinking", "thinking_content"] {
        match delta.get(key) {
            Some(Value::String(s)) => {
                if !echo_of_content(acc, s) {
                    push_reason(acc, s);
                }
            }
            Some(Value::Object(m)) => {
                if let Some(Value::String(s)) = m.get("content").or_else(|| m.get("text")) {
                    if !echo_of_content(acc, s) {
                        push_reason(acc, s);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(Value::Object(m)) = delta.get("reasoning") {
        if let Some(Value::String(s)) = m.get("content").or_else(|| m.get("text")) {
            if !echo_of_content(acc, s) {
                push_reason(acc, s);
            }
        }
    }
    if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
        for item in details {
            if let Some(s) = item
                .get("text")
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
            {
                if !echo_of_content(acc, s) {
                    push_reason(acc, s);
                }
            }
        }
    }
    let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) else {
        return;
    };
    for call in calls {
        let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let slot = acc
            .tools
            .entry(idx)
            .or_insert_with(|| (String::new(), String::new(), String::new()));
        if let Some(id) = call.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                slot.0 = id.to_string();
            }
        }
        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
            if !name.is_empty() {
                if slot.1.is_empty() {
                    slot.1 = name.to_string();
                } else if !slot.1.ends_with(name) {
                    slot.1.push_str(name);
                }
            }
        }
        if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
            slot.2.push_str(args);
        }
    }
}

pub(crate) fn finish_message(acc: &StreamAcc) -> Value {
    let mut calls: Vec<(usize, Value)> = acc
        .tools
        .iter()
        .map(|(i, (id, name, args))| {
            (
                *i,
                json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args }
                }),
            )
        })
        .collect();
    calls.sort_by_key(|(i, _)| *i);
    let tool_calls: Vec<Value> = calls.into_iter().map(|(_, v)| v).collect();
    let mut msg = json!({
        "role": "assistant",
        "content": acc.content,
    });
    if !acc.reasoning.is_empty() {
        msg["reasoning_content"] = json!(acc.reasoning);
        msg["reasoning"] = json!(acc.reasoning);
    }
    if !tool_calls.is_empty() {
        msg["tool_calls"] = json!(tool_calls);
    }
    if !acc.gemini_parts.is_empty() {
        msg["gemini_parts"] = json!(acc.gemini_parts);
    }
    if let Some(usage) = &acc.usage {
        msg["usage"] = usage.clone();
    }
    msg
}

pub(crate) async fn stream_complete(
    app: &AppHandle,
    role: &str,
    llm: &Llm,
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &str,
) -> Result<Value, String> {
    emit_think(app, role, "", false);
    let pruned = crate::context::prune_for_model(messages, llm.sees_images());
    if llm.uses_native_gemini() {
        let extra = llm.thinking_extra_for(role);
        let mut last_emit = Instant::now() - Duration::from_secs(1);
        let mut reason_grew = false;
        let mut last_reason = 0usize;
        return crate::provider::gemini_stream(
            llm,
            &pruned,
            tools,
            tool_choice,
            Some(&extra),
            role,
            |acc| {
                if acc.reasoning.len() > last_reason {
                    reason_grew = true;
                    last_reason = acc.reasoning.len();
                }
                if last_emit.elapsed() >= Duration::from_millis(80)
                    && emit_live_output(app, role, acc, false, reason_grew)
                {
                    last_emit = Instant::now();
                    reason_grew = false;
                }
            },
        )
        .await
        .map(|message| {
            emit_live_output(app, role, &{
                let mut done = StreamAcc::default();
                done.content = crate::coordinatore::message_text(&message);
                done.reasoning = message
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                done
            }, true, false);
            message
        });
    }
    let mut body = json!({
        "model": llm.model,
        "messages": pruned,
        "max_tokens": llm.max_tokens(),
        "stream": true,
        "stream_options": { "include_usage": true }
    });
    if let Value::Object(map) = llm.thinking_extra_for(role) {
        for (k, v) in map {
            body[k] = v;
        }
    }
    if tools.is_some() {
        body["tool_choice"] = json!(tool_choice);
    }
    crate::coordinatore::with_prompt_cache(
        &mut body,
        tools,
        &format!("puck-{role}"),
    );
    llm.prepare_body(&mut body);
    let mut response = llm
        .bind(llm.client.post(&llm.url))
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Network: {}", llm_http_err(e)))?;
    let status = response.status();
    if !status.is_success() {
        let payload: Value = response.json().await.unwrap_or(json!({}));
        return Err(provider_err(&payload, status, "LLM stream failed"));
    }

    let mut acc = StreamAcc::default();
    let mut buf = String::new();
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let mut done = false;
    let mut reason_grew = false;
    while !done {
        let chunk = match response.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => return Err(llm_http_err(e)),
        };
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
                done = true;
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(usage) = v.get("usage") {
                acc.usage = Some(usage.clone());
            }
            if let Some(delta) = v.pointer("/choices/0/delta") {
                let before = acc.reasoning.len();
                apply_delta(&mut acc, delta);
                if acc.reasoning.len() > before {
                    reason_grew = true;
                }
                if last_emit.elapsed() >= Duration::from_millis(80)
                    && emit_live_output(app, role, &acc, false, reason_grew)
                {
                    last_emit = Instant::now();
                    reason_grew = false;
                }
            }
        }
    }
    if reason_grew && !acc.reasoning.is_empty() {
        emit_think(app, role, &acc.reasoning, false);
    }
    emit_live_output(app, role, &acc, true, false);
    if acc.content.is_empty() && acc.tools.is_empty() && acc.reasoning.is_empty() {
        return Err("Empty stream.".into());
    }
    Ok(finish_message(&acc))
}

pub(crate) async fn complete_live(
    app: &AppHandle,
    role: &str,
    llm: &Llm,
    messages: &[Value],
    tools: Option<&[Value]>,
    tool_choice: &str,
) -> Result<Value, String> {
    match stream_complete(app, role, llm, messages, tools, tool_choice).await {
        Ok(message) => {
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
                    "via": if llm.uses_native_gemini() { "gemini-stream" } else { "stream" },
                    "role": role,
                    "tools": names,
                    "usage": crate::provider::log_usage(message.get("usage").unwrap_or(&json!({}))),
                    "text": crate::context::clip_log(&message_text(&message), 120),
                }),
            );
            Ok(message)
        }
        Err(e) => {
            crate::context::run_log(
                "llm",
                json!({
                    "ok": false,
                    "model": llm.model,
                    "via": "stream",
                    "role": role,
                    "err": crate::context::clip_log(&e, 360),
                }),
            );
            if is_timeout_msg(&e) {
                return Err(e);
            }
            let message = complete_with(
                llm,
                messages,
                tools,
                tool_choice,
                Some(llm.thinking_extra_for(role)),
            )
            .await?;
            let thought = message
                .get("reasoning_content")
                .or_else(|| message.get("reasoning"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let has_tools = !tool_calls(&message).is_empty();
            if !thought.is_empty() {
                emit_think(app, role, thought, true);
            }
            let spoken = message_text(&message);
            if !spoken.is_empty() && (role == "coordinatore" || !has_tools) {
                emit_speak(app, role, &spoken, true);
            }
            for call in tool_calls(&message) {
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let to = match name { "ask_coder" => "coder", _ => continue };
                let args = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let text = peek_brief_arg(args);
                if !text.is_empty() {
                    emit_brief(app, to, role, &text, true);
                }
            }
            Ok(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_delta_assembles_tools_and_thoughts() {
        let mut acc = StreamAcc::default();
        apply_delta(&mut acc, &json!({"reasoning": "look "}));
        apply_delta(&mut acc, &json!({"reasoning_content": "around"}));
        apply_delta(
            &mut acc,
            &json!({
                "tool_calls": [{
                    "index": 0,
                    "id": "c1",
                    "function": { "name": "read_file", "arguments": "{\"p" }
                }]
            }),
        );
        apply_delta(
            &mut acc,
            &json!({
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": "ath\":\"a.html\"}" }
                }]
            }),
        );
        let msg = finish_message(&acc);
        assert_eq!(msg["reasoning_content"], "look around");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "read_file");
        assert!(msg["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("a.html"));
    }

    #[test]
    fn google_thought_tags_leave_spoken_clean() {
        let mut acc = StreamAcc::default();
        apply_delta(&mut acc, &json!({"content": "<thought>plan "}));
        apply_delta(&mut acc, &json!({"content": "the page"}));
        apply_delta(&mut acc, &json!({"content": "</thought>Done."}));
        assert_eq!(acc.reasoning, "plan the page");
        assert_eq!(acc.content, "Done.");
    }

    #[test]
    fn stream_does_not_duplicate_the_same_thought() {
        let mut acc = StreamAcc::default();
        let thought = "The User says ciao. One short line.";
        apply_delta(&mut acc, &json!({"reasoning": thought, "reasoning_content": thought}));
        apply_delta(&mut acc, &json!({"reasoning": thought}));
        apply_delta(&mut acc, &json!({"reasoning_content": thought}));
        assert_eq!(acc.reasoning, thought);
    }

    #[test]
    fn thought_stays_while_reasoning_grows() {
        let mut acc = StreamAcc::default();
        apply_delta(&mut acc, &json!({"reasoning": "need the coder"}));
        assert!(!thought_should_hide(&acc, false, true));
        apply_delta(
            &mut acc,
            &json!({
                "tool_calls": [{
                    "index": 0,
                    "id": "c1",
                    "function": { "name": "patch_comune", "arguments": "{}" }
                }]
            }),
        );
        assert!(!thought_should_hide(&acc, false, true));
        assert!(!thought_should_hide(&acc, false, false));
        assert!(thought_should_hide(&acc, true, false));
        let mut spoken = StreamAcc::default();
        apply_delta(&mut spoken, &json!({"reasoning": "short"}));
        apply_delta(&mut spoken, &json!({"content": "Ciao."}));
        assert!(!thought_should_hide(&spoken, false, true));
        assert!(!thought_should_hide(&spoken, false, false));
        let mut only = StreamAcc::default();
        apply_delta(&mut only, &json!({"reasoning": "still"}));
        assert!(thought_should_hide(&only, true, false));
        assert!(!thought_should_hide(&only, false, false));
    }

    #[test]
    fn luna_echo_reasoning_is_not_thought() {
        let mut acc = StreamAcc::default();
        apply_delta(&mut acc, &json!({"content": "ciao"}));
        apply_delta(&mut acc, &json!({"reasoning": "ciao"}));
        assert_eq!(acc.content, "ciao");
        assert!(acc.reasoning.is_empty());
        assert!(should_speak("coordinatore", &acc));
    }

    #[test]
    fn speak_skips_content_that_repeats_reasoning() {
        let mut acc = StreamAcc::default();
        let thought = "**Reviewing New Content**\n\nI've received the colleague's latest submission.";
        apply_delta(&mut acc, &json!({"reasoning": thought}));
        apply_delta(&mut acc, &json!({"content": thought}));
        assert!(is_reasoning_echo(&acc.content, &acc.reasoning));
        assert!(!should_speak("coordinatore", &acc));
        apply_delta(&mut acc, &json!({"content": " Martedì alle 10."}));
        assert!(should_speak("coordinatore", &acc));
    }

    #[test]
    fn trades_speak_progress_even_with_tools() {
        let mut acc = StreamAcc::default();
        apply_delta(&mut acc, &json!({"content": "Comincio."}));
        assert!(should_speak("coder", &acc));
        assert!(should_speak("coordinatore", &acc));
        apply_delta(
            &mut acc,
            &json!({
                "tool_calls": [{
                    "index": 0,
                    "id": "c1",
                    "function": { "name": "view_image", "arguments": "{}" }
                }]
            }),
        );
        assert!(should_speak("coder", &acc));
        assert!(should_speak("coder", &acc));
        assert!(should_speak("coordinatore", &acc));
    }

    #[test]
    fn peek_brief_reads_complete_and_partial_json() {
        assert_eq!(
            peek_brief_arg(r#"{"brief":"Build the page."}"#),
            "Build the page."
        );
        assert_eq!(
            peek_brief_arg(r#"{"brief":"Build the page"#),
            "Build the page"
        );
        assert_eq!(peek_brief_arg(r#"{"brief":"line\nnext"}"#), "line\nnext");
        assert!(peek_brief_arg(r#"{"op":"replace"}"#).is_empty());
        assert_eq!(
            peek_json_field(r#"{"path":"index.html","content":"<h1>"#, "path"),
            "index.html"
        );
        assert_eq!(
            peek_json_field(r#"{"path":"index.html","content":"<h1>Hi"#, "content"),
            "<h1>Hi"
        );
        assert_eq!(
            tool_live_label("read_file", r#"{"path":"src/app.js"}"#).as_deref(),
            Some("Read src/app.js")
        );
        assert_eq!(
            tool_live_label("write_file", r#"{"path":"index.html"}"#),
            None
        );
    }
}
