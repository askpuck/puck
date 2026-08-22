use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::context::{compact_comune_if_needed, compact_project_if_needed};
use crate::coordinatore::{
    api_message, apply_comune, apply_project, comune_tool,
    complete, message_text, project_tool, split_calls, tool_call_name,
    tool_calls, AskCtx, Llm,
};
use crate::todo::TodoItem;

const SKIP: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "dist",
    ".DS_Store",
    ".puck-review",
    ".puck",
];
const MAX_READ: usize = 80_000;
const DEFAULT_READ_LINES: usize = 2000;
const MAX_WRITE: usize = 200_000;
const MAX_LIST: usize = 200;
const MAX_HITS: usize = 40;
const SNAP_MAX_FILES: usize = 150;
const SNAP_MAX_DEPTH: usize = 5;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Pulse {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    brief: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    patch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

fn emit(app: &AppHandle, pulse: Pulse) {
    let _ = app.emit("puck-crew", pulse);
}

fn emit_trace(app: &AppHandle, text: &str, path: Option<&str>) {
    emit(
        app,
        Pulse {
            kind: "trace",
            role: Some("coder".into()),
            text: Some(text.to_string()),
            brief: None,
            patch_id: None,
            path: path.map(|p| p.to_string()),
        },
    );
}

fn emit_wrote(app: &AppHandle, shown: &str, preview: &str) {
    emit(
        app,
        Pulse {
            kind: "wrote",
            role: Some("coder".into()),
            text: Some(preview.to_string()),
            brief: None,
            patch_id: None,
            path: Some(shown.to_string()),
        },
    );
}

pub fn load_dotenv() {
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.env");
    let _ = dotenvy::from_path(&from_crate);
    let _ = dotenvy::dotenv();
}

pub fn llm_coder() -> Result<Llm, String> {
    crate::coordinatore::llm_from_env(
        &["NANOGPT_CODER_MODEL", "NANOGPT_MODEL"],
        &[
            "OPENROUTER_CODER_MODEL",
            "OPENROUTER_MODEL",
            "NANOGPT_CODER_MODEL",
        ],
        &["GOOGLE_CODER_MODEL", "GOOGLE_MODEL"],
        &["DEEPSEEK_CODER_MODEL", "DEEPSEEK_MODEL"],
        &["CEREBRAS_CODER_MODEL", "CEREBRAS_MODEL"],
    )
}

pub fn workspace_root() -> Result<PathBuf, String> {
    crate::context::workspace_root()
}

fn canon(p: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn is_secret_path(path: &Path) -> bool {
    let home = PathBuf::from(env::var("HOME").unwrap_or_default());
    if !home.as_os_str().is_empty() {
        for name in [".ssh", ".gnupg", ".aws"] {
            if path.starts_with(home.join(name)) {
                return true;
            }
        }
        if path == home.join(".netrc") {
            return true;
        }
    }
    let puck_env = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.env");
    if path == puck_env {
        return true;
    }
    if let (Ok(a), Ok(b)) = (fs::canonicalize(path), fs::canonicalize(&puck_env)) {
        if a == b {
            return true;
        }
    }
    false
}

fn deny_secret(path: &Path) -> Result<(), String> {
    if is_secret_path(path) {
        return Err("That path is closed.".into());
    }
    Ok(())
}

pub(crate) fn jail(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let raw = rel.trim();
    if raw.contains('\0') {
        return Err("Bad path.".into());
    }
    let root = canon(root).unwrap_or_else(|_| root.to_path_buf());
    if raw.is_empty() || raw == "." {
        deny_secret(&root)?;
        return Ok(root);
    }
    let given = Path::new(raw);
    let out = if given.is_absolute() {
        given.to_path_buf()
    } else {
        root.join(given)
    };
    if out.exists() {
        let c = canon(&out)?;
        deny_secret(&c)?;
        return Ok(c);
    }
    deny_secret(&out)?;
    Ok(out)
}

pub(crate) fn path_is_allowed(root: &Path, asked: &str, allowed: &[String]) -> Result<bool, String> {
    if allowed.is_empty() {
        return Ok(false);
    }
    let want = jail(root, asked)?;
    for p in allowed {
        if jail(root, p).ok().as_ref() == Some(&want) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn rel_of(root: &Path, path: &Path) -> String {
    let root_c = canon(root).unwrap_or_else(|_| root.to_path_buf());
    let try_strip = |p: &Path| {
        p.strip_prefix(&root_c)
            .ok()
            .or_else(|| p.strip_prefix(root).ok())
            .map(|s| s.to_string_lossy().replace('\\', "/"))
            .filter(|s| !s.is_empty() && !s.starts_with('/'))
    };
    if let Some(s) = try_strip(path) {
        return s;
    }
    if let Ok(p) = canon(path) {
        if let Some(s) = try_strip(&p) {
            return s;
        }
        let shown = p.to_string_lossy().replace('\\', "/");
        if let Ok(home) = env::var("HOME") {
            let home = home.replace('\\', "/");
            if let Some(rest) = shown.strip_prefix(&home) {
                return format!("~{rest}");
            }
        }
        return shown;
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into())
}

fn skip_name(name: &str) -> bool {
    SKIP.iter().any(|s| name == *s)
}

pub(crate) fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

pub(crate) fn arg_bool(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
        }
        Some(Value::Number(n)) => n.as_u64() == Some(1) || n.as_i64() == Some(1),
        _ => false,
    }
}

pub(crate) fn arg_opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok())),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

pub(crate) fn list_dir(root: &Path, rel: &str) -> Result<String, String> {
    let dir = jail(root, rel)?;
    if !dir.is_dir() {
        return Err(format!("{} is not a folder.", rel_of(root, &dir)));
    }
    let mut names: Vec<String> = Vec::new();
    let mut rd = fs::read_dir(&dir).map_err(|e| e.to_string())?;
    while names.len() < MAX_LIST {
        let Some(ent) = rd.next() else { break };
        let ent = ent.map_err(|e| e.to_string())?;
        let name = ent.file_name().to_string_lossy().to_string();
        if skip_name(&name) {
            continue;
        }
        let mark = if ent.path().is_dir() { "/" } else { "" };
        names.push(format!("{name}{mark}"));
    }
    names.sort();
    if names.is_empty() {
        return Ok("(empty)".into());
    }
    Ok(names.join("\n"))
}

pub(crate) fn read_file(
    root: &Path,
    rel: &str,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<String, String> {
    let path = jail(root, rel)?;
    if !path.is_file() {
        return Err(format!("{} is not a file.", rel_of(root, &path)));
    }
    let bytes = fs::read(&path).map_err(|e| e.to_string())?;
    if bytes.contains(&0) {
        if crate::view::is_raster(&path) {
            return Err("This is an image. Call view_image on this path.".into());
        }
        return Err("Binary file. Do not read it.".into());
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = match offset {
        None | Some(0) | Some(1) => 0,
        Some(n) => (n as usize).saturating_sub(1),
    };
    if total == 0 {
        return Ok("lines 0-0 of 0\n(empty file)".into());
    }
    if offset.is_some() && start >= total {
        return Ok(format!("(file has {total} lines; offset is past the end)"));
    }
    let _ = limit;
    let take = DEFAULT_READ_LINES;
    let want_end = (start + take).min(total);
    let width = total.to_string().len().max(4);
    let mut numbered = String::new();
    let mut end = start;
    let mut cut_chars = false;
    for (i, line) in lines[start..want_end].iter().enumerate() {
        let n = start + i + 1;
        let row = format!("{n:>width$}|{line}\n");
        if numbered.len() + row.len() > MAX_READ {
            cut_chars = true;
            break;
        }
        numbered.push_str(&row);
        end = n;
    }
    if end == start && start < total {
        let n = start + 1;
        let line = lines[start];
        let cut: String = line.chars().take(MAX_READ.saturating_sub(20)).collect();
        numbered = format!("{n:>width$}|{cut}\n");
        end = n;
        cut_chars = line.len() > cut.len() || want_end > n;
    }
    let from = start + 1;
    let mut body = format!("lines {from}-{end} of {total}\n{numbered}");
    if end < total || cut_chars {
        body.push_str(&format!("… truncated. Pass offset={} to continue.", end + 1));
    }
    Ok(body)
}

pub(crate) fn read_start_line(offset: Option<u64>) -> usize {
    match offset {
        None | Some(0) | Some(1) => 1,
        Some(n) => n as usize,
    }
}

pub(crate) fn parse_read_span(body: &str) -> Option<(usize, usize, usize)> {
    let line = body.lines().next()?;
    let rest = line.strip_prefix("lines ")?;
    let (range, total_s) = rest.split_once(" of ")?;
    let (from_s, end_s) = range.split_once('-')?;
    let from = from_s.trim().parse().ok()?;
    let end = end_s.trim().parse().ok()?;
    let total = total_s
        .split_whitespace()
        .next()?
        .trim()
        .parse()
        .ok()?;
    Some((from, end, total))
}

pub(crate) fn read_again_note(from: usize, end: usize, total: usize) -> String {
    if end < total {
        format!(
            "Already in this thread (lines {from}-{end} of {total}). Do not read it again. Pass offset={} to continue.",
            end + 1
        )
    } else {
        format!("Already in this thread (lines {from}-{end} of {total}). Do not read it again.")
    }
}

pub(crate) fn search_tree(root: &Path, query: &str, rel: &str, use_regex: bool) -> Result<String, String> {
    let q = query.trim();
    if q.is_empty() {
        return Err("Empty search.".into());
    }
    let re = if use_regex {
        Some(Regex::new(q).map_err(|e| format!("Bad regex: {e}"))?)
    } else {
        None
    };
    let start = jail(root, rel)?;
    let mut hits: Vec<String> = Vec::new();
    fn walk(
        root: &Path,
        dir: &Path,
        q: &str,
        re: Option<&Regex>,
        hits: &mut Vec<String>,
    ) -> Result<(), String> {
        if hits.len() >= MAX_HITS {
            return Ok(());
        }
        let rd = fs::read_dir(dir).map_err(|e| e.to_string())?;
        for ent in rd {
            if hits.len() >= MAX_HITS {
                break;
            }
            let ent = ent.map_err(|e| e.to_string())?;
            let name = ent.file_name().to_string_lossy().to_string();
            if skip_name(&name) {
                continue;
            }
            let path = ent.path();
            if path.is_dir() {
                walk(root, &path, q, re, hits)?;
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            if bytes.contains(&0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (i, line) in text.lines().enumerate() {
                let hit = match re {
                    Some(re) => re.is_match(line),
                    None => line.contains(q),
                };
                if hit {
                    hits.push(format!("{}:{}:{line}", rel_of(root, &path), i + 1));
                    if hits.len() >= MAX_HITS {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
    walk(root, &start, q, re.as_ref(), &mut hits)?;
    if hits.is_empty() {
        return Ok("No matches.".into());
    }
    Ok(hits.join("\n"))
}

fn glob_to_regex(pat: &str) -> Result<Regex, String> {
    let mut out = String::from("^");
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        out.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        out.push_str(".*");
                        i += 2;
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            c => {
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out.push('$');
    Regex::new(&out).map_err(|e| format!("Bad glob: {e}"))
}

pub(crate) fn glob_files(root: &Path, pattern: &str) -> Result<String, String> {
    let pat = pattern.trim();
    if pat.is_empty() {
        return Err("Empty glob.".into());
    }
    if Path::new(pat).is_absolute() || pat.contains('\0') {
        return Err("Path must be relative to the workspace.".into());
    }
    if pat.split(['/', '\\']).any(|c| c == "..") {
        return Err("Path must be relative to the workspace.".into());
    }
    let expanded = if !pat.contains('/') && !pat.starts_with("**") {
        format!("**/{pat}")
    } else {
        pat.to_string()
    };
    let re = glob_to_regex(&expanded)?;
    let start = jail(root, ".")?;
    let mut hits: Vec<String> = Vec::new();
    let mut truncated = false;
    fn walk(
        root: &Path,
        dir: &Path,
        re: &Regex,
        hits: &mut Vec<String>,
        truncated: &mut bool,
    ) -> Result<(), String> {
        if hits.len() >= MAX_LIST {
            *truncated = true;
            return Ok(());
        }
        let rd = fs::read_dir(dir).map_err(|e| e.to_string())?;
        for ent in rd {
            if hits.len() >= MAX_LIST {
                *truncated = true;
                break;
            }
            let ent = ent.map_err(|e| e.to_string())?;
            let name = ent.file_name().to_string_lossy().to_string();
            if skip_name(&name) {
                continue;
            }
            let path = ent.path();
            if path.is_dir() {
                walk(root, &path, re, hits, truncated)?;
                continue;
            }
            let shown = rel_of(root, &path);
            if re.is_match(&shown) {
                hits.push(shown);
            }
        }
        Ok(())
    }
    walk(&start, &start, &re, &mut hits, &mut truncated)?;
    hits.sort();
    if hits.is_empty() {
        return Ok("No matches.".into());
    }
    let mut out = hits.join("\n");
    if truncated {
        out.push_str("\n… truncated");
    }
    Ok(out)
}

pub(crate) fn workspace_snapshot(root: &Path) -> String {
    if crate::context::is_computer_root(root) {
        return "No folder selected. Home is the computer root. Do not list the disk or write a project here — the User picks a folder first.".into();
    }
    let start = match jail(root, ".") {
        Ok(p) => p,
        Err(e) => return format!("Workspace: {e}"),
    };
    let mut files: Vec<String> = Vec::new();
    let mut truncated = false;
    fn walk(
        root: &Path,
        dir: &Path,
        depth: usize,
        files: &mut Vec<String>,
        truncated: &mut bool,
    ) -> Result<(), String> {
        if *truncated || files.len() >= SNAP_MAX_FILES {
            *truncated = true;
            return Ok(());
        }
        if depth > SNAP_MAX_DEPTH {
            return Ok(());
        }
        let mut ents: Vec<_> = fs::read_dir(dir)
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        ents.sort_by_key(|e| e.file_name());
        for ent in ents {
            if files.len() >= SNAP_MAX_FILES {
                *truncated = true;
                break;
            }
            let name = ent.file_name().to_string_lossy().to_string();
            if skip_name(&name) {
                continue;
            }
            let path = ent.path();
            if path.is_dir() {
                walk(root, &path, depth + 1, files, truncated)?;
            } else {
                files.push(rel_of(root, &path));
            }
        }
        Ok(())
    }
    if let Err(e) = walk(&start, &start, 0, &mut files, &mut truncated) {
        return format!("Workspace: {e}");
    }
    files.sort();
    if files.is_empty() {
        return "Workspace is empty.".into();
    }
    let mut out = String::from("Workspace files:\n");
    out.push_str(&files.join("\n"));
    if truncated {
        out.push_str("\n… truncated. glob or list_dir for a folder not listed.");
    }
    out
}

pub(crate) fn workspace_followup(root: &Path, note: &str) -> Value {
    json!({
        "role": "user",
        "content": format!("{}\n\n{note}", workspace_snapshot(root))
    })
}

pub(crate) enum RunRedirect {
    ListDir(String),
    ReadFile {
        path: String,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    Glob(String),
    Search {
        query: String,
        path: String,
        regex: bool,
    },
}

fn simple_argv(cmd: &str) -> Option<Vec<String>> {
    let t = cmd.trim();
    if t.is_empty() {
        return None;
    }
    if t.contains('|')
        || t.contains('>')
        || t.contains('<')
        || t.contains(';')
        || t.contains('&')
        || t.contains('`')
        || t.contains('$')
        || t.contains('\n')
    {
        return None;
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    for c in t.chars() {
        if let Some(qq) = q {
            if c == qq {
                q = None;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => q = Some(c),
            w if w.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if q.is_some() {
        return None;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn bin_name(s: &str) -> &str {
    Path::new(s)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(s)
}

pub(crate) fn redirect_run(command: &str) -> Option<RunRedirect> {
    let args = simple_argv(command)?;
    let bin = bin_name(args.first()?);
    match bin {
        "ls" => {
            let paths: Vec<&str> = args
                .iter()
                .skip(1)
                .map(|s| s.as_str())
                .filter(|a| !a.starts_with('-'))
                .collect();
            if paths.len() > 1 {
                return None;
            }
            Some(RunRedirect::ListDir(
                paths.first().copied().unwrap_or(".").to_string(),
            ))
        }
        "cat" => {
            let files: Vec<&str> = args
                .iter()
                .skip(1)
                .map(|s| s.as_str())
                .filter(|a| !a.starts_with('-'))
                .collect();
            if files.len() != 1 {
                return None;
            }
            Some(RunRedirect::ReadFile {
                path: files[0].to_string(),
                offset: None,
                limit: None,
            })
        }
        "head" => {
            let mut limit = Some(10u64);
            let mut path = None;
            let mut i = 1;
            while i < args.len() {
                let a = args[i].as_str();
                if a == "-n" {
                    i += 1;
                    limit = args.get(i).and_then(|s| s.parse().ok());
                } else if let Some(n) = a.strip_prefix("-n") {
                    if !n.is_empty() {
                        limit = n.parse().ok();
                    }
                } else if a.starts_with('-') && a.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
                    limit = a[1..].parse().ok();
                } else if a.starts_with('-') {
                    return None;
                } else if path.is_none() {
                    path = Some(a.to_string());
                } else {
                    return None;
                }
                i += 1;
            }
            Some(RunRedirect::ReadFile {
                path: path?,
                offset: None,
                limit,
            })
        }
        "find" => {
            let mut path = ".".to_string();
            let mut pattern: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "-name" | "-iname" => {
                        i += 1;
                        pattern = args.get(i).cloned();
                    }
                    "-type" => {
                        i += 1;
                    }
                    "-print" | "-print0" => {}
                    s if s.starts_with('-') => return None,
                    s => path = s.to_string(),
                }
                i += 1;
            }
            let pat = pattern?;
            let glob = if path == "." {
                pat
            } else {
                let path = path.trim_end_matches('/');
                if pat.contains('/') {
                    format!("{path}/{pat}")
                } else {
                    format!("{path}/**/{pat}")
                }
            };
            Some(RunRedirect::Glob(glob))
        }
        "grep" | "egrep" | "rg" => {
            let regex = bin != "grep" || args.iter().any(|a| a == "-E" || a == "-P");
            let mut query: Option<String> = None;
            let mut path = ".".to_string();
            let mut i = 1;
            while i < args.len() {
                let a = args[i].as_str();
                if a == "-e" {
                    i += 1;
                    query = args.get(i).cloned();
                } else if a.starts_with('-') {
                    i += 1;
                    continue;
                } else if query.is_none() {
                    query = Some(a.to_string());
                } else {
                    path = a.to_string();
                }
                i += 1;
            }
            Some(RunRedirect::Search {
                query: query?,
                path,
                regex,
            })
        }
        _ => None,
    }
}

pub(crate) fn apply_redirect(root: &Path, redir: RunRedirect) -> Result<String, String> {
    let (label, result) = match redir {
        RunRedirect::ListDir(p) => {
            let shown = if p.is_empty() { "." } else { p.as_str() };
            ("list_dir", list_dir(root, shown))
        }
        RunRedirect::ReadFile {
            path,
            offset,
            limit,
        } => ("read_file", read_file(root, &path, offset, limit)),
        RunRedirect::Glob(pat) => ("glob", glob_files(root, &pat)),
        RunRedirect::Search {
            query,
            path,
            regex,
        } => (
            "search",
            search_tree(root, &query, if path.is_empty() { "." } else { &path }, regex),
        ),
    };
    match result {
        Ok(s) => Ok(format!(
            "Used {label} instead of a shell listing/read/search. Prefer that tool next time.\n\n{s}"
        )),
        Err(e) => Err(e),
    }
}

fn apply_patch(old: &str, search: &str, replace: &str, all: bool) -> Result<String, String> {
    if search.is_empty() {
        return Err("Empty search. For a new file use write_file.".into());
    }
    let n = old.matches(search).count();
    if n == 0 {
        return Err("search not found. Read the file again.".into());
    }
    if all {
        return Ok(old.replace(search, replace));
    }
    if n > 1 {
        return Err(format!(
            "search found {n} times. Make it unique, or set replace_all true."
        ));
    }
    Ok(old.replacen(search, replace, 1))
}

fn write_now(
    app: &AppHandle,
    root: &Path,
    rel: &str,
    content: &str,
    log: &mut Vec<String>,
) -> Result<String, String> {
    if rel.trim().is_empty() {
        return Err("Missing path.".into());
    }
    if content.len() > MAX_WRITE {
        return Err("File too large.".into());
    }
    let path = jail(root, rel)?;
    let shown = rel_of(root, &path);
    let old = if path.is_file() {
        Some(fs::read_to_string(&path).map_err(|e| format!("{shown}: {e}"))?)
    } else if path.exists() {
        return Err(format!("{shown} is not a file."));
    } else {
        None
    };
    if old.as_deref() == Some(content) {
        return Ok(format!("No change: {shown}"));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{shown}: {e}"))?;
    }
    fs::write(&path, content).map_err(|e| format!("{shown}: {e}"))?;
    let preview = change_preview(&shown, old.as_deref(), content);
    log.push(format!("Wrote: {shown}"));
    emit_wrote(app, &shown, &preview);
    Ok(format!("Wrote: {shown}. The next read sees this version."))
}

fn patch_now(
    app: &AppHandle,
    root: &Path,
    rel: &str,
    search: &str,
    replace: &str,
    all: bool,
    log: &mut Vec<String>,
) -> Result<String, String> {
    if rel.trim().is_empty() {
        return Err("Missing path.".into());
    }
    let path = jail(root, rel)?;
    let shown = rel_of(root, &path);
    if !path.is_file() {
        return Err(format!(
            "{shown} is not a file. For a new file use write_file."
        ));
    }
    let old = fs::read_to_string(&path).map_err(|e| format!("{shown}: {e}"))?;
    if old.len() > MAX_WRITE {
        return Err("File too large.".into());
    }
    let new = apply_patch(&old, search, replace, all)?;
    if new.len() > MAX_WRITE {
        return Err("File too large.".into());
    }
    if old == new {
        return Ok(format!("No change: {shown}"));
    }
    fs::write(&path, &new).map_err(|e| format!("{shown}: {e}"))?;
    let preview = change_preview(&shown, Some(&old), &new);
    log.push(format!("Wrote: {shown}"));
    emit_wrote(app, &shown, &preview);
    Ok(format!("Wrote: {shown}. The next read sees this version."))
}

fn delete_now(
    app: &AppHandle,
    root: &Path,
    rel: &str,
    log: &mut Vec<String>,
) -> Result<String, String> {
    if rel.trim().is_empty() {
        return Err("Missing path.".into());
    }
    let path = jail(root, rel)?;
    let shown = rel_of(root, &path);
    if !path.exists() {
        return Err(format!("{shown} is not there."));
    }
    if !path.is_file() {
        return Err("delete_file is for files, not folders.".into());
    }
    fs::remove_file(&path).map_err(|e| format!("{shown}: {e}"))?;
    log.push(format!("Deleted: {shown}"));
    emit_trace(app, &format!("Deleted {shown}"), Some(&shown));
    Ok(format!("Deleted: {shown}."))
}

fn move_now(
    app: &AppHandle,
    root: &Path,
    from: &str,
    to: &str,
    log: &mut Vec<String>,
) -> Result<String, String> {
    let from = from.trim();
    let to = to.trim();
    if from.is_empty() || to.is_empty() {
        return Err("Missing from or to.".into());
    }
    let src = jail(root, from)?;
    let dest = jail(root, to)?;
    let a = rel_of(root, &src);
    let b = rel_of(root, &dest);
    if !src.is_file() {
        return Err(format!("{a} is not a file."));
    }
    if dest.exists() {
        return Err(format!(
            "{b} already exists. Delete it first if you mean to replace it."
        ));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{b}: {e}"))?;
    }
    fs::rename(&src, &dest).map_err(|e| format!("{a} → {b}: {e}"))?;
    log.push(format!("Moved: {a} → {b}"));
    emit_trace(app, &format!("Moved {a} → {b}"), Some(&b));
    Ok(format!("Moved: {a} → {b}."))
}

fn clip_preview(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return s.to_string();
    }
    format!(
        "{}\n… {} more lines",
        lines[..max_lines].join("\n"),
        lines.len() - max_lines
    )
}

fn line_hunk(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut i = 0;
    while i < a.len() && i < b.len() && a[i] == b[i] {
        i += 1;
    }
    let mut ja = a.len();
    let mut jb = b.len();
    while ja > i && jb > i && a[ja - 1] == b[jb - 1] {
        ja -= 1;
        jb -= 1;
    }
    let mut out = String::new();
    for line in &a[i..ja] {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &b[i..jb] {
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() {
        return "(change)".into();
    }
    clip_preview(out.trim_end(), 80)
}

fn change_preview(shown: &str, old: Option<&str>, new: &str) -> String {
    match old {
        None => format!("new file: {shown}\n{}", clip_preview(new, 40)),
        Some(old) if old == new => format!("{shown}: no change"),
        Some(old) => format!("{shown}\n{}", line_hunk(old, new)),
    }
}

pub(crate) fn replay(message: &Value) -> Value {
    let mut out = api_message(message);
    let thought = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .cloned();
    if let Some(r) = thought {
        if !r.is_null() && r.as_str() != Some("") {
            out["reasoning_content"] = r.clone();
            out["reasoning"] = r;
        }
    }
    out
}

fn coder_tools() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List one folder. Path is relative to the workspace, or absolute. Default is the workspace root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file. Path is relative to the workspace, or absolute. Always returns numbered lines. Always takes up to 2000 lines; a smaller limit is ignored. offset is the first line (1-based). Pass offset to continue a truncated file. Do not re-read the same path and offset.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "integer" },
                        "limit": { "type": "integer" }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "Find text in files. Default is a literal substring. Set regex true for a regular expression; a bad pattern is an error. Path is a folder to start from, default the workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "path": { "type": "string" },
                        "regex": { "type": "boolean" }
                    },
                    "required": ["query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files by name pattern. *.html matches anywhere in the workspace. src/*.rs stays one folder deep.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" }
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create a new file. Replace a whole file only if it does not exist yet, the brief says start over, or the page is wrong as a whole. Not for translations or term swaps on an existing file — use patch_file. Lands on disk now.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "patch_file",
                "description": "Preferred way to change a file that already exists. One change per call; send as many as the job needs in the same turn. Fast path for translations and term swaps — do not rewrite the file. Search must match once unless replace_all. Lands on disk now.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "search": { "type": "string" },
                        "replace": { "type": "string" },
                        "replace_all": { "type": "boolean" }
                    },
                    "required": ["path", "search", "replace"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "delete_file",
                "description": "Delete one file. Path is relative to the workspace, or absolute. Lands now. Not for folders.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "move_file",
                "description": "Rename or move a file. Paths are relative to the workspace, or absolute. Lands now. Fails if the destination already exists.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string" },
                        "to": { "type": "string" }
                    },
                    "required": ["from", "to"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "run",
                "description": "Run a shell command (install, test, fetch, git, build, open a file, anything the job needs). Auto. cwd is relative to the workspace or absolute, default the workspace. timeout is seconds (default 120, max 1800). background true starts it and returns while this job runs. Work in the workspace unless the job needs a path elsewhere. ~/.ssh is closed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "cwd": { "type": "string" },
                        "timeout": { "type": "integer" },
                        "background": { "type": "boolean" }
                    },
                    "required": ["command"]
                }
            }
        }),
        crate::image::tool(),
        json!({
            "type": "function",
            "function": {
                "name": "view_page",
                "description": "Open an HTML page in the workspace and take screenshots (phone 390px and desktop 1280px, or one width). Call this before you finish, and after a visual change. If the brief asked for the whole photo and the shot still crops, patch — do not finish. Do not re-open a page you already saw unless you changed it.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Workspace-relative .html file" },
                        "width": { "type": "integer", "description": "Optional. One width in pixels, 320–1920. Omit to capture phone and desktop." }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "view_image",
                "description": "See a png, jpg, webp, or gif in the workspace. Use this on logos, photos, and references the User dropped. SVG is text: call read_file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }
            }
        }),
        crate::todo::tool(),
        comune_tool(),
        project_tool(),
        crate::ask::tool(),
    ]
}

async fn complete_coder(
    app: &AppHandle,
    llm: &Llm,
    messages: &[Value],
    tools: &[Value],
) -> Result<Value, String> {
    complete(app, "coder", llm, messages, Some(tools), "auto").await
}


fn coder_standing(card_body: &str) -> String {
    card_body.trim_end().to_string()
}

pub async fn run_coder(
    app: &AppHandle,
    llm: &Llm,
    card_body: &str,
    packed: &crate::context::Packed,
    mut ask: Option<&mut AskCtx<'_>>,
    correction: bool,
) -> Result<String, String> {
    let root = workspace_root()?;
    let tools = coder_tools();
    let summary = packed.summary.clone();
    let mut todos: Vec<TodoItem> = Vec::new();
    let mut messages = crate::coordinatore::system_turns(&coder_standing(card_body), &summary);
    for turn in &packed.keep {
        let role = if turn.role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        messages.push(json!({
            "role": role,
            "content": turn.content
        }));
    }
    let last_brief = packed
        .keep
        .iter()
        .rev()
        .find(|t| t.role != "assistant")
        .map(|t| t.content.as_str())
        .unwrap_or("");
    let snap_note = if correction {
        "This wake is a correction. Apply the findings in the brief. Read only the files they name. Do not list_dir, glob, or ls the whole tree."
    } else {
        "This listing is current. Do not list_dir, glob, or ls to rediscover it. Read a file when you need its contents. Files already in the folder that this brief did not name are not this job."
    };
    messages.push(workspace_followup(&root, snap_note));
    let named = crate::view::load_named_images(&root, last_brief);
    if !named.is_empty() {
        if llm.sees_images() {
            messages.push(crate::view::images_followup(
                &named,
                "These files are named in the brief. Look at them. Call view_image if you need another look.",
            ));
        } else {
            let paths: Vec<&str> = named.iter().map(|s| s.shown.as_str()).collect();
            messages.push(json!({
                "role": "user",
                "content": format!(
                    "These files are named in the brief: {}. This model cannot see pixels. Do not call view_image. Use those paths in HTML src. SVG: read_file.",
                    paths.join(", ")
                )
            }));
        }
    }

    let mut log: Vec<String> = Vec::new();
    let mut live = crate::shell::LiveCmds::default();
    let mut final_text = String::new();
    let mut nudged_claim = false;
    let mut patched = false;
    let mut _wrote_html = false;
    let mut _saw_page = false;
    let mut _looked = false;
    let mut planned = false;
    let mut made_images: usize = 0;
    let mut seen_reads: HashMap<(String, usize), (usize, usize, usize)> = HashMap::new();

    for _ in 0..160 {
        crate::todo::pin_live(&mut messages, None, &todos);
        let message = complete_coder(app, llm, &messages, &tools).await?;
        let calls = tool_calls(&message);
        if calls.is_empty() {
            let spoken = message_text(&message);
            if log.is_empty() && claimed_land(&spoken) && !nudged_claim {
                nudged_claim = true;
                messages.push(replay(&message));
                messages.push(json!({
                    "role": "user",
                    "content": CLAIM_WRITE_NUDGE
                }));
                continue;
            }
            final_text = if log.is_empty() {
                strip_claimed_lands(&spoken)
            } else {
                spoken
            };
            break;
        }
        messages.push(replay(&message));
        let mut slots: Vec<(String, String)> = Vec::new();
        let mut round_shots = Vec::new();
        let mut touched_project = false;
        let (rest, comune_calls, people) = split_calls(calls);

        let mut image_calls: Vec<(String, Value)> = Vec::new();
        let mut rest_else: Vec<Value> = Vec::new();
        for call in rest {
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            if name == "create_image" {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let raw_args = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                crate::context::run_log(
                    "tool",
                    json!({
                        "role": "coder",
                        "name": "create_image",
                        "args": crate::context::clip_log(raw_args, 140),
                    }),
                );
                let args: Value = serde_json::from_str(raw_args).unwrap_or(json!({}));
                image_calls.push((id, args));
            } else {
                rest_else.push(call);
            }
        }
        if !image_calls.is_empty() {
            let names: Vec<String> = image_calls
                .iter()
                .map(|(_, a)| arg_str(a, "path"))
                .filter(|p| !p.is_empty())
                .collect();
            let label = if names.is_empty() {
                format!("Creating {} images", image_calls.len())
            } else {
                format!("Creating {}", names.join(", "))
            };
            emit_trace(app, &label, names.first().map(|s| s.as_str()));
            let remaining = crate::image::MAX_JOB_IMAGES.saturating_sub(made_images);
            let outcomes = crate::image::create_many(&root, image_calls, remaining).await;
            for out in outcomes {
                if let Some(ref shown) = out.shown {
                    made_images += 1;
                    log.push(format!("Wrote: {shown}"));
                    emit_trace(app, &format!("Created {shown}"), Some(shown));
                    if llm.sees_images() {
                        if let Ok((_, shot)) = crate::view::view_image(&root, shown) {
                            round_shots.push(shot);
                        }
                    }
                }
                slots.push((out.id, out.reply));
            }
        }

        for call in rest_else {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let raw_args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            crate::context::run_log(
                "tool",
                json!({
                    "role": "coder",
                    "name": name,
                    "args": crate::context::clip_log(raw_args, 140),
                }),
            );
            let args: Value = serde_json::from_str(raw_args).unwrap_or(json!({}));
            match name {
                "list_dir" => {
                    let p = arg_str(&args, "path");
                    let shown = if p.is_empty() { ".".into() } else { p.clone() };
                    emit_trace(app, &format!("Listed {shown}"), Some(&shown));
                    let r = list_dir(&root, if p.is_empty() { "." } else { &p });
                    slots.push((id, ok_or_err(r)));
                }
                "read_file" => {
                    let p = arg_str(&args, "path");
                    if !p.is_empty() {
                        emit_trace(app, &format!("Read {p}"), Some(&p));
                    }
                    let r = if p.is_empty() {
                        Err("Missing path.".into())
                    } else {
                        let offset = arg_opt_u64(&args, "offset");
                        let start = read_start_line(offset);
                        let key = (p.clone(), start);
                        if let Some(&(from, end, total)) = seen_reads.get(&key) {
                            Ok(read_again_note(from, end, total))
                        } else {
                            match read_file(&root, &p, offset, arg_opt_u64(&args, "limit")) {
                                Ok(body) => {
                                    if let Some(span) = parse_read_span(&body) {
                                        seen_reads.insert(key, span);
                                    }
                                    _looked = true;
                                    Ok(body)
                                }
                                Err(e) => Err(e),
                            }
                        }
                    };
                    slots.push((id, ok_or_err(r)));
                }
                "search" => {
                    let q = arg_str(&args, "query");
                    let p = arg_str(&args, "path");
                    emit_trace(
                        app,
                        &format!("Searched {q}"),
                        if p.is_empty() { None } else { Some(&p) },
                    );
                    let r = search_tree(
                        &root,
                        &q,
                        if p.is_empty() { "." } else { &p },
                        arg_bool(&args, "regex"),
                    );
                    slots.push((id, ok_or_err(r)));
                }
                "glob" => {
                    let pat = arg_str(&args, "pattern");
                    emit_trace(app, &format!("Looked for {pat}"), None);
                    slots.push((id, ok_or_err(glob_files(&root, &pat))));
                }
                "write_file" => {
                    let p = arg_str(&args, "path");
                    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                    if looks_like_html_path(&p) {
                        _wrote_html = true;
                    }
                    match write_now(app, &root, &p, content, &mut log) {
                        Ok(s) => {
                            slots.push((id, s));
                        }
                        Err(e) => slots.push((id, format!("Error: {e}"))),
                    }
                }
                "patch_file" => {
                    let p = arg_str(&args, "path");
                    let search = args.get("search").and_then(Value::as_str).unwrap_or("");
                    let replace = args.get("replace").and_then(Value::as_str).unwrap_or("");
                    if looks_like_html_path(&p) {
                        _wrote_html = true;
                    }
                    match patch_now(
                        app,
                        &root,
                        &p,
                        search,
                        replace,
                        arg_bool(&args, "replace_all"),
                        &mut log,
                    ) {
                        Ok(s) => {
                            slots.push((id, s));
                        }
                        Err(e) => slots.push((id, format!("Error: {e}"))),
                    }
                }
                "delete_file" => {
                    let p = arg_str(&args, "path");
                    match delete_now(app, &root, &p, &mut log) {
                        Ok(s) => {
                            slots.push((id, s));
                        }
                        Err(e) => slots.push((id, format!("Error: {e}"))),
                    }
                }
                "move_file" => {
                    let from = arg_str(&args, "from");
                    let to = arg_str(&args, "to");
                    if looks_like_html_path(&to) {
                        _wrote_html = true;
                    }
                    match move_now(app, &root, &from, &to, &mut log) {
                        Ok(s) => {
                            slots.push((id, s));
                        }
                        Err(e) => slots.push((id, format!("Error: {e}"))),
                    }
                }
                "run" => {
                    let command = args.get("command").and_then(Value::as_str).unwrap_or("");
                    let cwd = arg_str(&args, "cwd");
                    let shown = crate::shell::clip_cmd(command);
                    if !shown.is_empty() {
                        emit_trace(app, &format!("Ran {shown}"), None);
                    }
                    match crate::shell::run_cmd(
                        &root,
                        command,
                        &cwd,
                        crate::shell::timeout_secs(arg_opt_u64(&args, "timeout")),
                        arg_bool(&args, "background"),
                        &mut live,
                    ) {
                        Ok(r) => {
                            slots.push((id, r.output));
                        }
                        Err(e) => slots.push((id, format!("Error: {e}"))),
                    }
                }
                "todo_write" => {
                    let reply = crate::todo::apply(&mut todos, raw_args, app, "coder");
                    crate::todo::record_plan(&mut planned, &todos);
                    slots.push((id, reply));
                }
                "ask_user" => {
                    slots.push((
                        id,
                        crate::ask::run(app, "coder", raw_args).await,
                    ));
                }
                "view_page" => {
                    let p = arg_str(&args, "path");
                    let width = arg_opt_u64(&args, "width").map(|n| n as u32);
                    if p.is_empty() {
                        slots.push((id, "Error: Missing path.".into()));
                    } else {
                        emit_trace(app, &format!("Opened {p}"), Some(&p));
                        match crate::view::view_page(&root, &p, width) {
                            Ok((text, shots)) => {
                                _saw_page = true;
                                _looked = true;
                                if llm.sees_images() {
                                    round_shots.extend(shots);
                                }
                                slots.push((id, crate::view::seen_reply(text, llm.sees_images())));
                            }
                            Err(e) => slots.push((id, format!("Error: {e}"))),
                        }
                    }
                }
                "view_image" => {
                    let p = arg_str(&args, "path");
                    if p.is_empty() {
                        slots.push((id, "Error: Missing path.".into()));
                    } else {
                        emit_trace(app, &format!("Saw {p}"), Some(&p));
                        match crate::view::view_image(&root, &p) {
                            Ok((text, shot)) => {
                                _looked = true;
                                if llm.sees_images() {
                                    round_shots.push(shot);
                                }
                                slots.push((id, crate::view::seen_reply(text, llm.sees_images())));
                            }
                            Err(e) => slots.push((id, format!("Error: {e}"))),
                        }
                    }
                }
                other => slots.push((id, format!("Error: Unknown tool: {other}"))),
            }
        }

        for call in &comune_calls {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let name = tool_call_name(call);
            let reply = match &mut ask {
                Some(ctx) if name == "patch_project" => apply_project(ctx, "coder", args),
                Some(ctx) => apply_comune(ctx, "coder", args),
                None => "Error: cannot update memory from here.".into(),
            };
            if reply.starts_with("Common memory updated") {
                patched = true;
            }
            if reply.starts_with("Project memory updated") {
                touched_project = true;
            }
            slots.push((id, reply));
        }
        if patched {
            if let Some(ctx) = ask.as_mut() {
                match compact_comune_if_needed(ctx.app, ctx.llm, "coder").await {
                    Ok(true) => {
                        if let Some((_, content)) = slots.last_mut() {
                            content.push_str(" It went over the cap, so it was compacted to the current picture. Continue from here.");
                        }
                    }
                    Err(e) => {
                        if let Some((_, content)) = slots.last_mut() {
                            content.push_str(&format!(
                                " Compact failed ({e}). The file is still the long version."
                            ));
                        }
                    }
                    Ok(false) => {}
                }
            }
        }
        if touched_project {
            if let Some(ctx) = ask.as_mut() {
                match compact_project_if_needed(ctx.app, ctx.llm, "coder", &root).await {
                    Ok(true) => {
                        if let Some((_, content)) = slots.last_mut() {
                            content.push_str(" Project memory went over the cap, so it was compacted. Continue from here.");
                        }
                    }
                    Err(e) => {
                        if let Some((_, content)) = slots.last_mut() {
                            content.push_str(&format!(
                                " Project compact failed ({e}). The file is still the long version."
                            ));
                        }
                    }
                    Ok(false) => {}
                }
            }
        }

        for (id, content) in slots {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": content
            }));
        }
        if llm.sees_images() && !round_shots.is_empty() {
            messages.push(crate::view::images_followup(
                &round_shots,
                "Look at these images. Patch if the page is wrong. Then continue.",
            ));
        }
    }

    if !log.is_empty() {
        let _ = crate::context::write_schema(&root);
        return Ok(coder_land_report(&log, &final_text));
    }
    if final_text.is_empty() {
        return Err("Coder returned an empty note.".into());
    }
    let _ = crate::context::write_schema(&root);
    Ok(final_text)
}

const CLAIM_WRITE_NUDGE: &str = "You claimed a write. No write_file, patch_file, delete_file, or move_file landed this wake. Wrote: lines in older thread messages are other jobs. Call the file tool now. Do not recap a write you did not make.";

fn land_prefix(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("Wrote:")
        || t.starts_with("Deleted:")
        || t.starts_with("Moved:")
        || t.starts_with("Rejected:")
}

fn claimed_land(spoken: &str) -> bool {
    spoken.lines().any(land_prefix)
}

pub(crate) fn strip_claimed_lands(spoken: &str) -> String {
    spoken
        .lines()
        .filter(|l| !l.trim().is_empty() && !land_prefix(l))
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(dead_code)]
fn has_recap(spoken: &str) -> bool {
    spoken.lines().any(|l| !l.trim().is_empty() && !land_prefix(l))
}

fn coder_land_report(log: &[String], spoken: &str) -> String {
    // Solo il recap del modello: le righe "Wrote:" del log restano nei
    // tool, non arrivano all'utente (il Coordinatore riassume lui).
    let recap: String = spoken
        .lines()
        .filter(|l| !l.trim().is_empty() && !land_prefix(l))
        .collect::<Vec<_>>()
        .join("\n");
    if recap.is_empty() {
        log.join("\n")
    } else {
        recap
    }
}

fn looks_like_html_path(rel: &str) -> bool {
    let t = rel.to_ascii_lowercase();
    t.ends_with(".html") || t.ends_with(".htm")
}

fn ok_or_err(r: Result<String, String>) -> String {
    match r {
        Ok(s) => s,
        Err(e) => format!("Error: {e}"),
    }
}

#[derive(Deserialize)]
pub struct IncomingFile {
    pub name: String,
    pub data: String,
}

const MAX_INCOMING: usize = 12;
const MAX_INCOMING_BYTES: usize = 12 * 1024 * 1024;

fn safe_file_name(raw: &str) -> Result<String, String> {
    let name = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .trim();
    if name.is_empty() || name == "." || name == ".." {
        return Err("Bad file name.".into());
    }
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        return Err("Bad file name.".into());
    }
    Ok(cleaned)
}

fn unique_name(root: &Path, name: &str) -> String {
    if !root.join(name).exists() {
        return name.to_string();
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    for i in 2..100 {
        let cand = match ext {
            Some(e) => format!("{stem}-{i}.{e}"),
            None => format!("{stem}-{i}"),
        };
        if !root.join(&cand).exists() {
            return cand;
        }
    }
    format!("{}-{}", stem, std::process::id())
}

#[tauri::command]
pub fn save_workspace_files(files: Vec<IncomingFile>) -> Result<Vec<String>, String> {
    if files.is_empty() {
        return Err("No files.".into());
    }
    if files.len() > MAX_INCOMING {
        return Err("Too many files.".into());
    }
    let root = workspace_root()?;
    let mut saved = Vec::new();
    for file in files {
        let name = safe_file_name(&file.name)?;
        let bytes = crate::view::b64_decode(&file.data)?;
        if bytes.is_empty() {
            return Err(format!("{name} is empty."));
        }
        if bytes.len() > MAX_INCOMING_BYTES {
            return Err(format!("{name} is too large."));
        }
        let shown = unique_name(&root, &name);
        let path = jail(&root, &shown)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{shown}: {e}"))?;
        }
        fs::write(&path, &bytes).map_err(|e| format!("{shown}: {e}"))?;
        saved.push(shown);
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("puck-jail-{}", std::process::id()));
        let _ = fs::create_dir_all(&d);
        d
    }

    #[test]
    fn strip_claimed_lands_drops_fake_wrote() {
        let raw = "Wrote: deepfake/landing/index.html\nWrote: deepfake/landing/index.html\n\nFatto. Hero riorganizzato.";
        assert_eq!(strip_claimed_lands(raw), "Fatto. Hero riorganizzato.");
        assert!(claimed_land(raw));
        assert!(!claimed_land("Looked at the hero. Buttons still above."));
        assert_eq!(
            strip_claimed_lands("Looked at the hero."),
            "Looked at the hero."
        );
    }

    #[test]
    fn jail_allows_parent() {
        let root = tmp();
        assert!(jail(&root, "..").is_ok());
        assert!(jail(&root, "/etc/passwd").is_ok());
    }

    #[test]
    fn jail_blocks_secrets() {
        let root = tmp();
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        assert!(jail(&root, &format!("{home}/.ssh/id_rsa")).is_err());
        assert!(jail(&root, &format!("{home}/.gnupg/trustdb.gpg")).is_err());
    }

    #[test]
    fn path_is_allowed_matches_same_file_only() {
        let root = tmp();
        fs::write(root.join("index.html"), "<p>ok</p>").unwrap();
        fs::write(root.join("other.html"), "<p>no</p>").unwrap();
        let allowed = vec!["index.html".into()];
        assert!(path_is_allowed(&root, "index.html", &allowed).unwrap());
        assert!(path_is_allowed(&root, "./index.html", &allowed).unwrap());
        assert!(!path_is_allowed(&root, "other.html", &allowed).unwrap());
        assert!(!path_is_allowed(&root, "index.html", &[]).unwrap());
    }

    #[test]
    fn patch_needs_unique() {
        let old = "a\nb\na\n";
        assert!(apply_patch(old, "a", "x", false).is_err());
        assert_eq!(apply_patch("a\nb\n", "b", "c", false).unwrap(), "a\nc\n");
    }

    #[test]
    fn patch_replace_all() {
        let old = "a\nb\na\n";
        assert_eq!(apply_patch(old, "a", "x", true).unwrap(), "x\nb\nx\n");
        assert!(apply_patch(old, "z", "x", true).is_err());
    }

    #[test]
    fn glob_blocks_parent() {
        let root = tmp();
        assert!(glob_files(&root, "../secret").is_err());
        assert!(glob_files(&root, "/etc/passwd").is_err());
    }

    #[test]
    fn glob_star_html_is_recursive() {
        let re = glob_to_regex("**/*.html").unwrap();
        assert!(re.is_match("a.html"));
        assert!(re.is_match("x/a.html"));
        assert!(re.is_match("x/y/a.html"));
        assert!(!re.is_match("a.rs"));
        assert!(!re.is_match("a.html.bak"));
    }

    #[test]
    fn glob_one_folder_stays_shallow() {
        let re = glob_to_regex("src/*.rs").unwrap();
        assert!(re.is_match("src/main.rs"));
        assert!(!re.is_match("src/a/main.rs"));
        assert!(!re.is_match("main.rs"));
    }

    #[test]
    fn list_dir_shows_dotfiles() {
        let root = std::env::temp_dir().join(format!("puck-dot-{}", std::process::id()));
        let _ = fs::create_dir_all(&root);
        fs::write(root.join(".gitignore"), "x\n").unwrap();
        fs::write(root.join("a.txt"), "a\n").unwrap();
        let out = list_dir(&root, ".").unwrap();
        assert!(out.contains(".gitignore"), "{out}");
        assert!(out.contains("a.txt"), "{out}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn glob_finds_nested_html() {
        let root = std::env::temp_dir().join(format!("puck-glob-{}", std::process::id()));
        let nested = root.join("pages");
        let _ = fs::create_dir_all(&nested);
        fs::write(root.join("home.html"), "<p>a</p>").unwrap();
        fs::write(nested.join("about.html"), "<p>b</p>").unwrap();
        fs::write(root.join("skip.rs"), "fn x() {}").unwrap();
        let out = glob_files(&root, "*.html").unwrap();
        assert!(out.contains("home.html"));
        assert!(out.contains("pages/about.html"));
        assert!(!out.contains("skip.rs"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_regex_errors_on_bad_pattern() {
        let root = tmp();
        let err = search_tree(&root, "(", ".", true).unwrap_err();
        assert!(err.contains("Bad regex"));
    }

    #[test]
    fn search_literal_does_not_treat_query_as_regex() {
        let root = std::env::temp_dir().join(format!("puck-search-{}", std::process::id()));
        let _ = fs::create_dir_all(&root);
        fs::write(root.join("a.txt"), "price is $5\n").unwrap();
        let hits = search_tree(&root, "$5", ".", false).unwrap();
        assert!(hits.contains("$5"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_offset_limit_is_one_indexed() {
        let root = std::env::temp_dir().join(format!("puck-read-{}", std::process::id()));
        let _ = fs::create_dir_all(&root);
        fs::write(root.join("n.txt"), "a\nb\nc\nd\n").unwrap();
        let out = read_file(&root, "n.txt", Some(2), Some(2)).unwrap();
        assert!(out.contains("lines 2-4 of 4"), "{out}");
        assert!(out.contains("|b"));
        assert!(out.contains("|c"));
        assert!(out.contains("|d"));
        let whole = read_file(&root, "n.txt", None, None).unwrap();
        assert!(whole.contains("1|a") || whole.contains("   1|a"));
        assert!(whole.contains("lines 1-4 of 4"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_ignores_small_limit() {
        let root = std::env::temp_dir().join(format!("puck-read-lim-{}", std::process::id()));
        let _ = fs::create_dir_all(&root);
        let body: String = (1..=160).map(|n| format!("line-{n}\n")).collect();
        fs::write(root.join("p.txt"), body).unwrap();
        let out = read_file(&root, "p.txt", None, Some(100)).unwrap();
        assert!(out.contains("lines 1-160 of 160"), "{out}");
        assert!(out.contains("line-160"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_again_note_covers_whole_file() {
        let note = read_again_note(1, 160, 160);
        assert!(note.contains("Already in this thread"));
        assert!(note.contains("1-160 of 160"));
        assert!(!note.contains("Pass offset"));
        let more = read_again_note(1, 2000, 4000);
        assert!(more.contains("offset=2001"));
    }

    #[test]
    fn parse_read_span_from_header() {
        assert_eq!(parse_read_span("lines 1-160 of 160\n   1|hi\n"), Some((1, 160, 160)));
        assert_eq!(read_start_line(None), 1);
        assert_eq!(read_start_line(Some(1)), 1);
        assert_eq!(read_start_line(Some(40)), 40);
    }

    #[test]
    fn redirect_run_maps_ls_cat_find_grep() {
        match redirect_run("ls -la .").unwrap() {
            RunRedirect::ListDir(p) => assert_eq!(p, "."),
            _ => panic!("ls"),
        }
        match redirect_run("cat src/main.rs").unwrap() {
            RunRedirect::ReadFile { path, .. } => assert_eq!(path, "src/main.rs"),
            _ => panic!("cat"),
        }
        match redirect_run("find . -name *.html").unwrap() {
            RunRedirect::Glob(p) => assert_eq!(p, "*.html"),
            _ => panic!("find"),
        }
        match redirect_run("grep -n hello src").unwrap() {
            RunRedirect::Search { query, path, regex } => {
                assert_eq!(query, "hello");
                assert_eq!(path, "src");
                assert!(!regex);
            }
            _ => panic!("grep"),
        }
        assert!(redirect_run("ls | wc").is_none());
        assert!(redirect_run("npm test").is_none());
    }

    #[test]
    fn workspace_snapshot_skips_computer_root() {
        let out = workspace_snapshot(Path::new("/"));
        assert!(out.contains("No folder selected"), "{out}");
        assert!(!out.contains("Applications"), "{out}");
    }

    #[test]
    fn workspace_snapshot_lists_nested() {
        let root = std::env::temp_dir().join(format!("puck-snap-{}", std::process::id()));
        let nested = root.join("pages");
        let _ = fs::create_dir_all(&nested);
        fs::write(root.join("home.html"), "<p>a</p>").unwrap();
        fs::write(nested.join("about.html"), "<p>b</p>").unwrap();
        let out = workspace_snapshot(&root);
        assert!(out.contains("home.html"), "{out}");
        assert!(out.contains("pages/about.html"), "{out}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rel_of_new_file_is_short() {
        let root = tmp();
        let path = root.join("prova.txt");
        assert_eq!(rel_of(&root, &path), "prova.txt");
    }

    #[test]
    fn hunk_shows_only_the_change() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let h = line_hunk(old, new);
        assert!(h.contains("- b"));
        assert!(h.contains("+ B"));
        assert!(!h.contains("- a"));
    }

    #[test]
    fn safe_file_name_strips_paths() {
        assert_eq!(safe_file_name("logo.png").unwrap(), "logo.png");
        assert_eq!(safe_file_name("/tmp/../logo.png").unwrap(), "logo.png");
        assert!(safe_file_name("..").is_err());
        assert!(safe_file_name("").is_err());
    }

    #[test]
    fn unique_name_adds_suffix() {
        let root = std::env::temp_dir().join(format!("puck-uniq-{}", std::process::id()));
        let _ = fs::create_dir_all(&root);
        fs::write(root.join("logo.png"), b"a").unwrap();
        assert_eq!(unique_name(&root, "logo.png"), "logo-2.png");
        assert_eq!(unique_name(&root, "other.png"), "other.png");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_file_points_images_at_view() {
        let root = std::env::temp_dir().join(format!("puck-bin-{}", std::process::id()));
        let _ = fs::create_dir_all(&root);
        fs::write(root.join("logo.png"), [0x89, b'P', b'N', b'G', 0, 1]).unwrap();
        let err = read_file(&root, "logo.png", None, None).unwrap_err();
        assert!(err.contains("view_image"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }
}
