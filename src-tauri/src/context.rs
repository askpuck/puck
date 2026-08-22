use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::coordinatore::{complete_tokens, message_text, ChatTurn, Llm};

const DEFAULT_RAW: usize = 256_000;
const DEFAULT_SUMMARY: usize = 64_000;
const DEFAULT_PACK: usize = 400_000;
const DEFAULT_KEEP_EX: usize = 20;
const DEFAULT_KEEP_MIN: usize = 3;
const DEFAULT_KEEP_TOK: usize = 40_000;
const DEFAULT_API_MAX: usize = 400_000;
const DEFAULT_API_RESERVE: usize = 32_000;
const DEFAULT_PRUNE_KEEP_TURNS: usize = 4;
const STUB_HEAD_CHARS: usize = 800;
const STUB_HEAD_LINES: usize = 12;
const DEFAULT_COMUNE: usize = 100_000;
const DEFAULT_COMUNE_KEEP: usize = 20_000;
const DEFAULT_PROJECT_MAX: usize = 160_000;
const DEFAULT_PROJECT_KEEP: usize = 128_000;
const CHUNK_TOKENS: usize = 60_000;

#[derive(Default)]
pub struct Memory {
    pub summaries: HashMap<String, String>,
}

impl Memory {
    pub fn load() -> Self {
        let mut summaries = HashMap::new();
        let Ok(dir) = summary_dir() else {
            return Self { summaries };
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return Self { summaries };
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if safe_id(id).is_none() {
                continue;
            }
            if let Ok(text) = fs::read_to_string(&path) {
                summaries.insert(id.to_string(), text);
            }
        }
        Self { summaries }
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
}

fn emit_pack(app: &AppHandle, kind: &'static str, thread_id: &str, text: Option<&str>) {
    let _ = app.emit(
        "puck-crew",
        CrewPulse {
            kind,
            role: Some(thread_id.to_string()),
            text: text.map(ToOwned::to_owned),
        },
    );
}

fn env_usize(name: &str, default: usize) -> usize {
    crate::coordinatore::env_puck(name)
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

fn safe_id(id: &str) -> Option<&str> {
    if id.is_empty() || id.len() > 64 {
        return None;
    }
    if id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Some(id)
    } else {
        None
    }
}

pub fn context_dir() -> Result<PathBuf, String> {
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.env");
    let _ = dotenvy::from_path(&from_crate);
    let _ = dotenvy::dotenv();
    if let Some(raw) = crate::coordinatore::env_puck("PUCK_CONTEXT") {
        let p = PathBuf::from(raw.trim());
        if !p.as_os_str().is_empty() {
            fs::create_dir_all(&p).map_err(|e| format!("Context: {e}"))?;
            return Ok(p);
        }
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../context");
    fs::create_dir_all(&dir).map_err(|e| format!("Context: {e}"))?;
    Ok(dir)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub path: String,
    pub name: String,
    pub exists: bool,
    pub source: String,
    pub chosen: bool,
}

fn workspace_state_path() -> Result<PathBuf, String> {
    Ok(context_dir()?.join("workspace.json"))
}

fn path_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn info_of(path: &Path, source: &str, exists: bool) -> WorkspaceInfo {
    WorkspaceInfo {
        path: path.to_string_lossy().into_owned(),
        name: path_name(path),
        exists,
        source: source.into(),
        chosen: !is_computer_root(path),
    }
}

pub(crate) fn computer_root() -> PathBuf {
    PathBuf::from("/")
}

pub(crate) fn is_computer_root(p: &Path) -> bool {
    let n = p.to_string_lossy().replace('\\', "/");
    n == "/" || n == "//"
}

fn load_saved_workspace() -> Option<PathBuf> {
    let path = workspace_state_path().ok()?;
    let raw = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let s = v.get("path")?.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(PathBuf::from(s))
}

fn save_saved_workspace(path: &Path) -> Result<(), String> {
    let file = workspace_state_path()?;
    let raw = serde_json::to_vec_pretty(&json!({ "path": path.to_string_lossy() }))
        .map_err(|e| format!("Workspace: {e}"))?;
    write_atomic(&file, &raw)
}

pub fn workspace_info() -> Result<WorkspaceInfo, String> {
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.env");
    let _ = dotenvy::from_path(&from_crate);
    let _ = dotenvy::dotenv();
    if let Some(raw) = crate::coordinatore::env_puck("PUCK_WORKSPACE") {
        let p = PathBuf::from(raw.trim());
        if !p.as_os_str().is_empty() {
            if !p.is_dir() {
                fs::create_dir_all(&p).map_err(|e| format!("Workspace: {e}"))?;
            }
            let p = fs::canonicalize(&p).unwrap_or(p);
            return Ok(info_of(&p, "env", p.is_dir()));
        }
    }
    if let Some(saved) = load_saved_workspace() {
        let exists = saved.is_dir();
        let shown = if exists {
            fs::canonicalize(&saved).unwrap_or(saved)
        } else {
            saved
        };
        return Ok(info_of(&shown, "saved", exists));
    }
    let dir = computer_root();
    Ok(info_of(&dir, "default", dir.is_dir()))
}

pub fn workspace_root() -> Result<PathBuf, String> {
    let info = workspace_info()?;
    if !info.exists {
        return Err("Workspace folder is missing. Pick another.".into());
    }
    Ok(PathBuf::from(info.path))
}

pub fn working_folder_blurb() -> String {
    let Ok(info) = workspace_info() else {
        return String::new();
    };
    if !info.exists || !info.chosen {
        return format!(
            "## Working folder\n\nNo project is open. Choose with open_project: project=\"new: <name>\" to create one, or project=\"<slug>\" to open it (list them with look_project what=list). The Coder works only inside the open project. Do not point the User at a folder picker."
        );
    }
    let in_vault = vault_root()
        .map(|v| Path::new(&info.path).starts_with(&v))
        .unwrap_or(false);
    if !in_vault {
        return format!(
            "## Working folder\n\nOpen project: {} — but it is outside the vault ({}). Move work into the vault: create the project with open_project and do the work there. The vault is the only place Puck keeps project memory.",
            info.name, info.path
        );
    }
    format!(
        "## Working folder\n\n{} ({}) — this is the open project: relative paths, file tools, run cwd, and .puck/memory.md are this folder. Projects are created or switched with open_project (Identity: .puck/memory.md, tree: .puck/schema.md). Chat history may mention earlier folders; those files are not this job unless the User's current order says so.",
        info.path, info.name
    )
}

#[tauri::command]
pub fn get_workspace() -> Result<WorkspaceInfo, String> {
    workspace_info()
}

#[tauri::command]
pub fn set_workspace(path: String) -> Result<WorkspaceInfo, String> {
    let p = PathBuf::from(path.trim());
    if p.as_os_str().is_empty() {
        return Err("No folder.".into());
    }
    if !p.is_dir() {
        return Err("That is not a folder.".into());
    }
    let p = fs::canonicalize(&p).map_err(|e| format!("Workspace: {e}"))?;
    save_saved_workspace(&p)?;
    workspace_info()
}

fn turns_dir() -> Result<PathBuf, String> {
    let dir = context_dir()?.join("turns");
    fs::create_dir_all(&dir).map_err(|e| format!("Context: {e}"))?;
    Ok(dir)
}

fn summary_dir() -> Result<PathBuf, String> {
    let dir = context_dir()?.join("summary");
    fs::create_dir_all(&dir).map_err(|e| format!("Context: {e}"))?;
    Ok(dir)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "Context: bad path".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("Context: {e}"))?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("puck")
    ));
    fs::write(&tmp, bytes).map_err(|e| format!("Context: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("Context: {e}"))?;
    Ok(())
}

static RUN_LOG: Mutex<()> = Mutex::new(());

pub fn clip_log(s: &str, max: usize) -> String {
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
    format!("{out}…")
}

fn log_stamp() -> String {
    std::process::Command::new("date")
        .args(["+%Y-%m-%d %H:%M:%S"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".into())
}

pub fn run_log(event: &str, fields: Value) {
    let mut rec = json!({
        "ts": log_stamp(),
        "event": event,
    });
    if let Value::Object(map) = fields {
        if let Some(obj) = rec.as_object_mut() {
            for (k, v) in map {
                obj.insert(k, v);
            }
        }
    }
    let line = rec.to_string();
    eprintln!("[puck] {line}");
    let Ok(_g) = RUN_LOG.lock() else {
        return;
    };
    let Ok(dir) = context_dir() else {
        return;
    };
    let path = dir.join("run.jsonl");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn turns_path(thread_id: &str) -> Result<PathBuf, String> {
    let id = safe_id(thread_id).ok_or_else(|| "Context: bad thread id".to_string())?;
    Ok(turns_dir()?.join(format!("{id}.json")))
}

fn summary_path(thread_id: &str) -> Result<PathBuf, String> {
    let id = safe_id(thread_id).ok_or_else(|| "Context: bad thread id".to_string())?;
    Ok(summary_dir()?.join(format!("{id}.md")))
}

pub fn load_turns(thread_id: &str) -> Vec<ChatTurn> {
    let Ok(path) = turns_path(thread_id) else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save_turns(thread_id: &str, turns: &[ChatTurn]) {
    let Ok(path) = turns_path(thread_id) else {
        return;
    };
    let Ok(raw) = serde_json::to_vec_pretty(turns) else {
        return;
    };
    let _ = write_atomic(&path, &raw);
}

pub fn load_summary_file(thread_id: &str) -> String {
    let Ok(path) = summary_path(thread_id) else {
        return String::new();
    };
    fs::read_to_string(&path).unwrap_or_default()
}

pub fn save_summary(thread_id: &str, text: &str) {
    let Ok(path) = summary_path(thread_id) else {
        return;
    };
    let _ = write_atomic(path.as_path(), text.as_bytes());
}

pub fn absorb(disk: Vec<ChatTurn>, incoming: Vec<ChatTurn>) -> Vec<ChatTurn> {
    if incoming.is_empty() {
        return disk;
    }
    if disk.is_empty() {
        return incoming;
    }
    if incoming.len() >= disk.len() {
        return incoming;
    }
    let mut out = disk;
    for turn in incoming {
        if out
            .last()
            .is_some_and(|x| x.role == turn.role && x.content == turn.content)
        {
            continue;
        }
        out.push(turn);
    }
    out
}

pub fn absorb_thread(thread_id: &str, incoming: Vec<ChatTurn>) -> Vec<ChatTurn> {
    let merged = absorb(load_turns(thread_id), incoming);
    save_turns(thread_id, &merged);
    merged
}

pub fn fill_threads_from_disk(live: &mut HashMap<String, Vec<ChatTurn>>) {
    let Ok(dir) = turns_dir() else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if safe_id(id).is_none() || id == "coordinatore" {
            continue;
        }
        if live.contains_key(id) {
            continue;
        }
        let turns = load_turns(id);
        if !turns.is_empty() {
            live.insert(id.to_string(), turns);
        }
    }
}

pub fn persist_after_ask(
    main_id: &str,
    history: &[ChatTurn],
    reply: &str,
    live: &HashMap<String, Vec<ChatTurn>>,
) {
    let mut main = history.to_vec();
    if !reply.is_empty() {
        let dup = main
            .last()
            .is_some_and(|t| t.role == "assistant" && t.content == reply);
        if !dup {
            main.push(ChatTurn {
                role: "assistant".into(),
                content: reply.to_string(),
            });
        }
    }
    save_turns(main_id, &main);
    for (id, turns) in live {
        save_turns(id, turns);
    }
}

const COMUNE_TEMPLATE: &str = "# User

Facts about the User: who they are, what they run, how they like to work, standing preferences and numbers of theirs. Everyone reads and edits this whole file. Keep it current: replace or rewrite what changed, delete what is dead, add only what is new. Not a diary.

## Workspaces

Short notes on folders you have worked in. One `###` per folder: the folder name and its full path. Two to four lines: what that work was, what still matters if you come back. Not the live picture of the current folder (that is .puck/memory.md). Not a task list. Drop a folder when it is dead.
";

const PROJECT_TEMPLATE: &str = "# Project

## Structure
(tree: files and folders that matter, what each is for. Realign on first look at this folder, and after any file change.)

## What this is
(one or two lines: what this project is and for whom. This line is the identity the Coordinator reads across the vault - keep it current.)

## Done
(what is already true on disk)

## Missing
(what is still open, holes, placeholders, unvalidated facts)
";

pub fn vault_root() -> Result<PathBuf, String> {
    let from_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.env");
    let _ = dotenvy::from_path(&from_crate);
    let _ = dotenvy::dotenv();
    let p = if let Some(raw) = crate::coordinatore::env_puck("PUCK_VAULT") {
        PathBuf::from(raw.trim())
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("puck-lavori")
    };
    if p.as_os_str().is_empty() {
        return Err("Vault: empty path.".into());
    }
    fs::create_dir_all(&p).map_err(|e| format!("Vault: {e}"))?;
    Ok(p.canonicalize().unwrap_or(p))
}

pub fn slug_ok(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 60
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn project_dir(slug: &str) -> Result<PathBuf, String> {
    if !slug_ok(slug) {
        return Err(format!("Bad project name: {slug}"));
    }
    let p = vault_root()?.join(slug);
    if !p.is_dir() {
        return Err(format!("No project {slug}. List projects with look_project what=list."));
    }
    Ok(p)
}

pub fn create_project(slug: &str) -> Result<PathBuf, String> {
    if !slug_ok(slug) {
        return Err(format!("Bad project name: {slug}"));
    }
    let p = vault_root()?.join(slug);
    if p.exists() {
        return Err(format!("Project {slug} already exists."));
    }
    let puck = p.join(".puck");
    fs::create_dir_all(&puck).map_err(|e| format!("Vault: {e}"))?;
    write_atomic(&puck.join("memory.md"), PROJECT_TEMPLATE.as_bytes())?;
    write_atomic(&puck.join("schema.md"), "(no files yet)\n".as_bytes())?;
    Ok(p)
}

pub fn project_identity(root: &Path) -> String {
    let Ok(raw) = fs::read_to_string(root.join(".puck").join("memory.md")) else {
        return String::new();
    };
    let mut in_what = false;
    for line in raw.lines() {
        let t = line.trim();
        if t.starts_with("## ") {
            in_what = t.eq_ignore_ascii_case("## What this is") || t.eq_ignore_ascii_case("## Cosa è");
            continue;
        }
        if in_what && !t.is_empty() && !t.starts_with("(") {
            return t.chars().take(160).collect();
        }
    }
    String::new()
}

pub struct ProjectInfo {
    pub slug: String,
    pub path: String,
    pub identity: String,
    pub modified: String,
}

pub fn list_projects() -> Result<Vec<ProjectInfo>, String> {
    let v = vault_root()?;
    let mut out = Vec::new();
    for entry in fs::read_dir(&v).map_err(|e| format!("Vault: {e}"))? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !path.is_dir() || name.starts_with('.') || !slug_ok(name) {
            continue;
        }
        let identity = project_identity(&path);
        let modified = fs::metadata(path.join(".puck").join("memory.md"))
            .or_else(|_| fs::metadata(&path))
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| -> String {
                let d: chrono::DateTime<chrono::Local> = t.into();
                d.format("%Y-%m-%d").to_string()
            })
            .unwrap_or_default();
        out.push(ProjectInfo {
            slug: name.to_string(),
            path: path.display().to_string(),
            identity,
            modified,
        });
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(out)
}

/// Schema of the project folder (what is on disk), written to .puck/schema.md.
pub fn write_schema(root: &Path) -> Result<(), String> {
    let mut lines: Vec<String> = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<String>) {
        if depth > 3 {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut items: Vec<(bool, String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with('.') || n == "node_modules" || n == "target" {
                    return None;
                }
                Some((p.is_dir(), n, p))
            })
            .collect();
        items.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        for (is_dir, n, p) in items {
            let suffix = if is_dir { "/" } else { "" };
            out.push(format!("{}{}{}", "  ".repeat(depth), n, suffix));
            if is_dir {
                walk(&p, depth + 1, out);
            }
        }
    }
    walk(root, 0, &mut lines);
    let text = if lines.is_empty() {
        "(no files yet)\n".to_string()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    write_atomic(&root.join(".puck").join("schema.md"), text.as_bytes())
}

fn user_memory_path() -> Result<PathBuf, String> {
    Ok(vault_root()?.join("user.md"))
}

fn comune_path() -> Result<PathBuf, String> {
    user_memory_path()
}

fn comune_max() -> usize {
    env_usize("PUCK_COMUNE_MAX_TOKENS", DEFAULT_COMUNE)
}

fn comune_keep() -> usize {
    env_usize("PUCK_COMUNE_KEEP_TOKENS", DEFAULT_COMUNE_KEEP)
}

fn project_max() -> usize {
    env_usize("PUCK_PROJECT_MAX_TOKENS", DEFAULT_PROJECT_MAX)
}

fn project_keep() -> usize {
    env_usize("PUCK_PROJECT_KEEP_TOKENS", DEFAULT_PROJECT_KEEP)
}

fn apply_comune_to(file: &str, ops: &[PatchOp]) -> Result<String, String> {
    if ops.is_empty() {
        return Err("Empty patch.".into());
    }
    let ops: Vec<PatchOp> = ops
        .iter()
        .map(|op| {
            let mut op = op.clone();
            if op.op == "invalidate" {
                op.op = "delete".into();
            }
            if op.op == "write" {
                op.op = "rewrite".into();
            }
            op
        })
        .collect();
    for op in &ops {
        match op.op.as_str() {
            "rewrite" => {
                if op
                    .text
                    .as_deref()
                    .or(op.new.as_deref())
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                {
                    return Err(
                        "rewrite needs text: the whole markdown file, including ## Structure, ## What this is, ## Done, ## Missing."
                            .into(),
                    );
                }
            }
            "create" => {
                if !file.trim().is_empty() {
                    return Err(
                        "File already has content. Use op=rewrite with text=the whole markdown file (## Structure, ## What this is, ## Done, ## Missing)."
                            .into(),
                    );
                }
            }
            "add" | "replace" | "delete" | "remove" | "rewrite_section" => {}
            other => {
                return Err(format!(
                    "Unknown op {other}. Use rewrite (whole file), rewrite_section, replace, delete, remove (by id), or add."
                ))
            }
        }
    }
    Ok(apply_ops_inner(file, &ops, "m", true))
}

fn ensure_workspaces_heading(s: &str) -> Option<String> {
    let low = s.to_ascii_lowercase();
    if low.contains("## workspaces") || low.contains("## cartelle") || low.contains("## folders")
    {
        return None;
    }
    Some(format!(
        "{}\n\n## Workspaces\n\nShort notes on folders you have worked in. One `###` per folder: the folder name and its full path. Two to four lines: what that work was, what still matters if you come back. Not the live picture of the current folder (that is .puck/memory.md). Not a task list. Drop a folder when it is dead.\n",
        s.trim_end()
    ))
}

fn migrate_user_heading(s: &str) -> Option<String> {
    if !s.contains("# Owner") && !s.contains("Facts about the Owner") {
        return None;
    }
    Some(
        s.replace("# Owner\n", "# User\n")
            .replace("Facts about the Owner", "Facts about the User"),
    )
}

fn looks_like_old_skeleton(s: &str) -> bool {
    ((s.contains("## Titolare") && s.contains("## Coordinatore"))
        || (s.contains("## Owner") && s.contains("## Coordinator"))
        || (s.contains("## User") && s.contains("## Coordinator")))
        && s.contains("## Coder")
        && estimate_tokens(s) < 500
}

pub fn load_comune() -> String {
    let Ok(path) = comune_path() else {
        return COMUNE_TEMPLATE.to_string();
    };
    match fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => {
            if looks_like_old_skeleton(&s) {
                let _ = write_atomic(&path, COMUNE_TEMPLATE.as_bytes());
                COMUNE_TEMPLATE.to_string()
            } else {
                let mut out = s;
                let mut dirty = false;
                if let Some(migrated) = migrate_user_heading(&out) {
                    out = migrated;
                    dirty = true;
                }
                if let Some(filled) = ensure_workspaces_heading(&out) {
                    out = filled;
                    dirty = true;
                }
                if dirty {
                    let _ = write_atomic(&path, out.as_bytes());
                }
                out
            }
        }
        _ => {
            // First run with the vault: migrate the old context/comune.md if it exists.
            if let Ok(dir) = context_dir() {
                let legacy = dir.join("comune.md");
                if let Ok(old) = fs::read_to_string(&legacy) {
                    if !old.trim().is_empty() && old.trim() != COMUNE_TEMPLATE.trim() {
                        let _ = write_atomic(&path, old.as_bytes());
                        return old;
                    }
                }
            }
            let _ = write_atomic(&path, COMUNE_TEMPLATE.as_bytes());
            COMUNE_TEMPLATE.to_string()
        }
    }
}

fn save_comune(text: &str) {
    let Ok(path) = comune_path() else {
        return;
    };
    let _ = write_atomic(&path, text.as_bytes());
}

fn parse_patch_ops(args: &str) -> Result<Vec<PatchOp>, String> {
    let v: Value = serde_json::from_str(args).unwrap_or(json!({}));
    let ops = if let Some(arr) = v.get("ops").and_then(Value::as_array) {
        serde_json::from_value::<Vec<PatchOp>>(json!(arr))
            .map_err(|e| format!("Bad patch: {e}"))?
    } else {
        let op = v
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("add")
            .to_string();
        vec![PatchOp {
            op,
            old: v.get("old").and_then(Value::as_str).map(ToOwned::to_owned),
            new: v.get("new").and_then(Value::as_str).map(ToOwned::to_owned),
            text: v.get("text").and_then(Value::as_str).map(ToOwned::to_owned),
            heading: v
                .get("heading")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            id: v.get("id").and_then(Value::as_str).map(ToOwned::to_owned),
        }]
    };
    if ops.is_empty() {
        return Err("Empty patch.".into());
    }
    Ok(ops)
}

fn commit_patch(
    path: &Path,
    current: &str,
    ops: &[PatchOp],
    log_name: &str,
    ok: &str,
) -> Result<String, String> {
    let before = estimate_tokens(current);
    let out = apply_comune_to(current, ops)?;
    let after = estimate_tokens(&out);
    write_atomic(path, out.as_bytes())?;
    let names: Vec<String> = ops.iter().map(|o| o.op.clone()).collect();
    run_log(
        log_name,
        json!({
            "ok": true,
            "ops": names,
            "tokens_before": before,
            "tokens_after": after,
        }),
    );
    Ok(ok.into())
}

pub fn patch_comune(args: &str) -> Result<String, String> {
    let ops = parse_patch_ops(args)?;
    let file = load_comune();
    commit_patch(
        &comune_path()?,
        &file,
        &ops,
        "comune",
        "Common memory updated.",
    )
}

pub fn project_path(root: &Path) -> PathBuf {
    root.join(".puck").join("memory.md")
}

pub fn load_project(root: &Path) -> String {
    let path = project_path(root);
    match fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => PROJECT_TEMPLATE.to_string(),
    }
}

pub fn load_project_now() -> String {
    match workspace_root() {
        Ok(root) => load_project(&root),
        Err(_) => String::new(),
    }
}

fn heading_names() -> [(&'static str, &'static [&'static str]); 4] {
    [
        ("Structure", &["structure", "struttura"]),
        (
            "What this is",
            &["what this is", "understood", "picture", "cos'è", "quadro"],
        ),
        ("Done", &["done", "fatto", "landed"]),
        ("Missing", &["missing", "manca", "open", "holes"]),
    ]
}

fn heading_key(line: &str) -> Option<String> {
    let t = line.trim();
    let t = t.strip_prefix("##")?.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.to_ascii_lowercase())
}

fn section_text(file: &str, aliases: &[&str]) -> String {
    let lines: Vec<&str> = file.lines().collect();
    let mut start = None;
    for (i, line) in lines.iter().enumerate() {
        let Some(key) = heading_key(line) else {
            continue;
        };
        if aliases.iter().any(|a| key == *a) {
            start = Some(i + 1);
            break;
        }
    }
    let start = match start {
        Some(s) => s,
        None => return String::new(),
    };
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start) {
        if heading_key(line).is_some() {
            end = i;
            break;
        }
    }
    lines[start..end].join("\n")
}

fn is_stub_section(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return true;
    }
    if t.starts_with('(') && t.ends_with(')') {
        return true;
    }
    let low = t.to_ascii_lowercase();
    low.contains("realign on first look")
        || low.contains("enough depth that the next role")
        || low.contains("already true on disk")
        || low.contains("unvalidated facts")
}

pub(crate) fn project_picture_ok(file: &str) -> Result<(), String> {
    let need = heading_names();
    let mut missing: Vec<&str> = Vec::new();
    let mut thin: Vec<&str> = Vec::new();
    for (label, aliases) in need {
        let body = section_text(file, aliases);
        if body.is_empty() {
            missing.push(label);
            continue;
        }
        if is_stub_section(&body) || body.chars().filter(|c| !c.is_whitespace()).count() < 40 {
            thin.push(label);
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "Too thin. Missing: {}. For a first look use op=rewrite with text=the whole file, headings ## Structure, ## What this is, ## Done, ## Missing. Do not use add — it appends and leaves the stub empty.",
            missing.join(", ")
        ));
    }
    if !thin.is_empty() {
        return Err(format!(
            "Too thin. Rewrite {} with what you actually saw or changed. Use op=rewrite for the whole file, or rewrite_section for one heading. Structure must be the real tree. What this is / Done / Missing must be in enough depth that the next role does not rediscover the folder. Do not invent prices or counts.",
            thin.join(", ")
        ));
    }
    Ok(())
}

pub fn patch_project(root: &Path, args: &str) -> Result<String, String> {
    let ops = parse_patch_ops(args)?;
    let file = load_project(root);
    let out = apply_comune_to(&file, &ops)?;
    if out == file {
        return Err(
            "Nothing changed. replace/delete did not match, or add left the file as-is. For a first look or a full realign, use op=rewrite with text=the whole markdown file, including ## Structure, ## What this is, ## Done, ## Missing."
                .into(),
        );
    }
    if ops.iter().all(|o| o.op == "add") && project_picture_ok(&out).is_err() {
        return Err(
            "add appends and leaves the stub headings empty. Use op=rewrite with text=the whole file, headings ## Structure, ## What this is, ## Done, ## Missing."
                .into(),
        );
    }
    project_picture_ok(&out)?;
    commit_patch(
        &project_path(root),
        &file,
        &ops,
        "project",
        "Project memory updated.",
    )
}

pub fn project_needs_compact(root: &Path) -> bool {
    estimate_tokens(&load_project(root)) > project_max()
}

pub fn comune_needs_compact() -> bool {
    estimate_tokens(&load_comune()) > comune_max()
}

fn chunk_markdown(file: &str, max: usize) -> Vec<String> {
    if estimate_tokens(file) <= max {
        return vec![file.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for line in file.lines() {
        let next = if cur.is_empty() {
            line.to_string()
        } else {
            format!("{cur}\n{line}")
        };
        if !cur.is_empty() && estimate_tokens(&next) > max {
            chunks.push(std::mem::take(&mut cur));
            cur = line.to_string();
        } else {
            cur = next;
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    if chunks.is_empty() {
        vec![file.to_string()]
    } else {
        chunks
    }
}

fn strip_md_fence(raw: &str) -> String {
    let t = raw.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    let rest = rest
        .strip_prefix("markdown")
        .or_else(|| rest.strip_prefix("md"))
        .unwrap_or(rest)
        .trim_start_matches('\n');
    if let Some(end) = rest.rfind("```") {
        rest[..end].trim().to_string()
    } else {
        rest.trim().to_string()
    }
}

const COMPACT_COMUNE_SYSTEM: &str = r###"You rewrite User memory (comune.md). You are not the Coordinator.

Output the new markdown file only. No JSON. No preamble. No fence.

Keep: facts about the User (who they are, what they run, their hours, how they work, standing preferences), and short notes on folders worked in (folder name + full path).
Drop: live folder progress that belongs in .puck/memory.md, diary, who-did-what, duplicates, stale, chat, prose, task backlogs.

Same language as the source. The file must stay current User memory, at most 20000 tokens.
"###;

const COMPACT_PROJECT_SYSTEM: &str = r###"You rewrite this folder's memory (.puck/memory.md). You are not the Coordinator.

Output the new markdown file only. No JSON. No preamble. No fence.

Keep these headings, in this order: Structure, What this is, Done, Missing.
Keep: the tree of the folder (realigned), what you understood in depth, what is already true on disk, what is still open.
Drop: User prefs, diary, who-did-what, duplicates, stale, chat, two-line recaps.

Same language as the source. The file must stay a current picture of this folder, at most 20000 tokens.
"###;

async fn compact_memory_chunk(
    llm: &Llm,
    picture: &str,
    overflow: &str,
    system: &str,
    empty_label: &str,
) -> Result<String, String> {
    let keep = comune_keep();
    let user = if picture.trim().is_empty() {
        format!("The file is empty. Write {empty_label} from this material.\n\n{overflow}")
    } else {
        format!(
            "Current file:\n\n{picture}\n\n---\nAbsorb this older material. Rewrite so it stays current. Do not keep diary.\n\n{overflow}"
        )
    };
    let messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": user}),
    ];
    let message = complete_tokens(llm, &messages, None, "none").await?;
    let raw = strip_md_fence(&message_text(&message));
    if raw.is_empty() {
        return Err(format!("Empty {empty_label} compact reply"));
    }
    Ok(cap_tokens(&raw, keep))
}

async fn compact_comune_chunk(llm: &Llm, picture: &str, overflow: &str) -> Result<String, String> {
    compact_memory_chunk(llm, picture, overflow, COMPACT_COMUNE_SYSTEM, "comune.md").await
}

async fn compact_project_chunk(llm: &Llm, picture: &str, overflow: &str) -> Result<String, String> {
    compact_memory_chunk(
        llm,
        picture,
        overflow,
        COMPACT_PROJECT_SYSTEM,
        ".puck/memory.md",
    )
    .await
}

async fn rewrite_comune(llm: &Llm, file: &str) -> Result<String, String> {
    let keep = comune_keep();
    let mut picture = String::new();
    for chunk in chunk_markdown(file, CHUNK_TOKENS) {
        picture = compact_comune_chunk(llm, &picture, &chunk).await?;
    }
    if estimate_tokens(&picture) > keep {
        picture = compact_comune_chunk(llm, "", &picture).await?;
    }
    Ok(cap_tokens(&picture, keep))
}

pub async fn compact_comune_if_needed(
    app: &AppHandle,
    llm: &Llm,
    role: &str,
) -> Result<bool, String> {
    if !comune_needs_compact() {
        return Ok(false);
    }
    let before = estimate_tokens(&load_comune());
    run_log(
        "compact",
        json!({ "phase": "start", "role": role, "tokens": before }),
    );
    emit_pack(app, "pack", role, Some("comune"));
    let file = load_comune();
    let result = rewrite_comune(llm, &file).await;
    emit_pack(app, "packed", role, Some("comune"));
    match result {
        Ok(out) => {
            let after = estimate_tokens(&out);
            save_comune(&out);
            run_log(
                "compact",
                json!({
                    "phase": "done",
                    "role": role,
                    "tokens_before": before,
                    "tokens_after": after,
                }),
            );
            Ok(true)
        }
        Err(e) => {
            run_log(
                "compact",
                json!({
                    "phase": "fail",
                    "role": role,
                    "tokens": before,
                    "err": clip_log(&e, 180),
                }),
            );
            Err(e)
        }
    }
}

async fn rewrite_project(llm: &Llm, file: &str) -> Result<String, String> {
    let keep = project_keep();
    let mut picture = String::new();
    for chunk in chunk_markdown(file, CHUNK_TOKENS) {
        picture = compact_project_chunk(llm, &picture, &chunk).await?;
    }
    if estimate_tokens(&picture) > keep {
        picture = compact_project_chunk(llm, "", &picture).await?;
    }
    Ok(cap_tokens(&picture, keep))
}

pub async fn compact_project_if_needed(
    app: &AppHandle,
    llm: &Llm,
    role: &str,
    root: &Path,
) -> Result<bool, String> {
    if !project_needs_compact(root) {
        return Ok(false);
    }
    let before = estimate_tokens(&load_project(root));
    run_log(
        "compact_project",
        json!({ "phase": "start", "role": role, "tokens": before }),
    );
    emit_pack(app, "pack", role, Some("project"));
    let file = load_project(root);
    let result = rewrite_project(llm, &file).await;
    emit_pack(app, "packed", role, Some("project"));
    match result {
        Ok(out) => {
            let after = estimate_tokens(&out);
            write_atomic(&project_path(root), out.as_bytes())?;
            run_log(
                "compact_project",
                json!({
                    "phase": "done",
                    "role": role,
                    "tokens_before": before,
                    "tokens_after": after,
                }),
            );
            Ok(true)
        }
        Err(e) => {
            run_log(
                "compact_project",
                json!({
                    "phase": "fail",
                    "role": role,
                    "tokens": before,
                    "err": clip_log(&e, 180),
                }),
            );
            Err(e)
        }
    }
}

pub fn estimate_tokens(text: &str) -> usize {
    let n = text.chars().count();
    (n + 2) / 3
}

fn turns_tokens(turns: &[ChatTurn]) -> usize {
    turns.iter().map(|t| estimate_tokens(&t.content) + 4).sum()
}

fn keep_start(turns: &[ChatTurn]) -> usize {
    if turns.is_empty() {
        return 0;
    }
    let keep_ex = env_usize("PUCK_KEEP_EXCHANGES", DEFAULT_KEEP_EX);
    let keep_min = env_usize("PUCK_KEEP_MIN_EXCHANGES", DEFAULT_KEEP_MIN).min(keep_ex);
    let keep_max = env_usize("PUCK_KEEP_MAX_TOKENS", DEFAULT_KEEP_TOK);

    let max_msgs = keep_ex.saturating_mul(2).min(turns.len());
    let min_msgs = keep_min.saturating_mul(2).min(turns.len()).max(1);
    let mut start = turns.len().saturating_sub(max_msgs);

    while start + min_msgs < turns.len() && turns_tokens(&turns[start..]) > keep_max {
        start += 2;
        if start >= turns.len() {
            start = turns.len() - 1;
            break;
        }
    }
    if start >= turns.len() {
        turns.len() - 1
    } else {
        start
    }
}

fn needs_compact(summary: &str, turns: &[ChatTurn]) -> bool {
    let raw_limit = env_usize("PUCK_COMPACT_RAW_TOKENS", DEFAULT_RAW);
    let pack_max = env_usize("PUCK_PACK_MAX_TOKENS", DEFAULT_PACK);
    let raw = turns_tokens(turns);
    let sum = estimate_tokens(summary);
    raw >= raw_limit || raw + sum >= pack_max
}

fn cap_summary(file: &str) -> String {
    cap_tokens(file, env_usize("PUCK_SUMMARY_MAX_TOKENS", DEFAULT_SUMMARY))
}

fn cap_tokens(file: &str, max: usize) -> String {
    if estimate_tokens(file) <= max {
        return file.to_string();
    }
    let mut out = String::new();
    for line in file.lines() {
        let next = if out.is_empty() {
            line.to_string()
        } else {
            format!("{out}\n{line}")
        };
        if estimate_tokens(&next) > max {
            break;
        }
        out = next;
    }
    if out.is_empty() {
        file.chars().take(max.saturating_mul(3)).collect()
    } else {
        out
    }
}

fn next_id_for(file: &str, prefix: &str) -> u32 {
    let needle = format!("[{prefix}-");
    let mut max = 0u32;
    let mut rest = file;
    while let Some(pos) = rest.find(&needle) {
        rest = &rest[pos + needle.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u32>() {
            max = max.max(n);
        }
        if rest.is_empty() {
            break;
        }
        rest = &rest[1.min(rest.len())..];
    }
    max + 1
}

fn with_ids(text: &str, id: &mut u32, prefix: &str) -> String {
    let needle = format!("[{prefix}-");
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('-') && !trimmed.contains(&needle) {
            let body = trimmed.trim_start_matches('-').trim();
            out.push_str(&format!("- [{prefix}-{id}] {body}\n"));
            *id += 1;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn count_sub(hay: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    hay.matches(needle).count()
}

fn rewrite_section(file: &str, heading: &str, body: &str) -> Result<String, String> {
    let heading = heading.trim();
    let start = file.find(heading).ok_or_else(|| format!("No section {heading}"))?;
    let after = start + heading.len();
    let end = file[after..]
        .find("\n## ")
        .map(|i| after + i)
        .unwrap_or(file.len());
    let mut out = String::new();
    out.push_str(&file[..after]);
    out.push('\n');
    out.push_str(body.trim());
    out.push('\n');
    out.push_str(&file[end..]);
    Ok(out)
}

#[derive(Debug, Clone, Deserialize)]
struct PatchOp {
    op: String,
    #[serde(default)]
    old: Option<String>,
    #[serde(default)]
    new: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    heading: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PatchDoc {
    ops: Vec<PatchOp>,
}

fn parse_patch(raw: &str) -> Result<Vec<PatchOp>, String> {
    let trimmed = raw.trim();
    let json_str = if let Some(start) = trimmed.find("```") {
        let after = trimmed[start + 3..].trim_start();
        let after = after
            .strip_prefix("json")
            .unwrap_or(after)
            .trim_start();
        let end = after.find("```").unwrap_or(after.len());
        after[..end].trim().to_string()
    } else if let Some(start) = trimmed.find('{') {
        let end = trimmed.rfind('}').ok_or("Incomplete JSON patch")?;
        trimmed[start..=end].to_string()
    } else {
        return Err("No JSON in compact reply".into());
    };
    let doc: PatchDoc =
        serde_json::from_str(&json_str).map_err(|e| format!("Bad patch JSON: {e}"))?;
    Ok(doc.ops)
}

fn apply_ops(file: &str, ops: &[PatchOp]) -> String {
    cap_summary(&apply_ops_inner(file, ops, "f", true))
}

fn push_chunk(out: &mut String, chunk: &str) {
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push_str(chunk);
}

fn line_for_id(file: &str, id: &str) -> Option<String> {
    let needle = format!("[{id}]");
    file.lines()
        .find(|l| l.contains(&needle))
        .map(ToOwned::to_owned)
}

fn line_token(line: &str) -> Option<String> {
    let s = line.find('[')?;
    let e = line[s..].find(']')? + s + 1;
    Some(line[s..e].to_string())
}

fn resolve_old(out: &str, op: &PatchOp) -> Option<String> {
    if let Some(old) = op.old.as_deref().filter(|s| !s.is_empty()) {
        return Some(old.to_string());
    }
    line_for_id(out, op.id.as_deref()?)
}

fn apply_ops_inner(file: &str, ops: &[PatchOp], prefix: &str, assign_ids: bool) -> String {
    let mut out = file.to_string();
    let mut id = next_id_for(&out, prefix);
    for op in ops {
        match op.op.as_str() {
            "rewrite" => {
                if let Some(text) = op.text.as_deref().or(op.new.as_deref()) {
                    let mut t = text.to_string();
                    if !t.ends_with('\n') {
                        t.push('\n');
                    }
                    out = if assign_ids {
                        with_ids(&t, &mut id, prefix)
                    } else {
                        t
                    };
                }
            }
            "create" => {
                if let Some(text) = &op.text {
                    if out.trim().is_empty() {
                        out = if assign_ids {
                            with_ids(text, &mut id, prefix)
                        } else {
                            text.clone()
                        };
                    }
                }
            }
            "add" => {
                if let Some(text) = &op.text {
                    let mut chunk = text.clone();
                    if let Some(id_ref) = &op.id {
                        let tok = format!("[{id_ref}]");
                        if !chunk.contains(&tok) {
                            chunk = format!("- {tok} {}\n", chunk.trim());
                        }
                    }
                    let chunk = if assign_ids {
                        with_ids(&chunk, &mut id, prefix)
                    } else {
                        let mut t = chunk;
                        if !t.ends_with('\n') {
                            t.push('\n');
                        }
                        t
                    };
                    push_chunk(&mut out, &chunk);
                }
            }
            "replace" => {
                let Some(old) = resolve_old(&out, op) else {
                    continue;
                };
                let mut new = op.new.as_deref().unwrap_or("").to_string();
                if let Some(id_ref) = &op.id {
                    let tok = format!("[{id_ref}]");
                    if !new.contains(&tok) {
                        new = format!("- {tok} {}", new.trim());
                    }
                }
                if count_sub(&out, &old) == 1 {
                    out = out.replacen(&old, &new, 1);
                } else if let Some(heading) = &op.heading {
                    if let Ok(rewritten) = rewrite_section(&out, heading, &new) {
                        out = rewritten;
                    }
                }
            }
            "delete" => {
                let Some(old) = resolve_old(&out, op) else {
                    continue;
                };
                if count_sub(&out, &old) == 1 {
                    out = out.replacen(&old, "", 1);
                }
            }
            "remove" => {
                let Some(old) = resolve_old(&out, op) else {
                    continue;
                };
                if count_sub(&out, &old) == 1 {
                    out = out.replacen(&old, "", 1);
                }
            }
            "invalidate" => {
                let Some(old) = resolve_old(&out, op) else {
                    continue;
                };
                if count_sub(&out, &old) == 1 && !old.contains("~~") {
                    let marked = format!("~~{old}~~");
                    out = out.replacen(&old, &marked, 1);
                }
            }
            "rewrite_section" => {
                let Some(heading) = op.heading.as_deref() else {
                    continue;
                };
                let body = op.text.as_deref().or(op.new.as_deref()).unwrap_or("");
                if let Ok(rewritten) = rewrite_section(&out, heading, body) {
                    out = rewritten;
                }
            }
            _ => {}
        }
    }
    out
}

fn render_turns(turns: &[ChatTurn]) -> String {
    let mut out = String::new();
    for turn in turns {
        out.push_str(&format!("{}:\n{}\n\n", turn.role, turn.content));
    }
    out
}

fn chunk_turns(turns: &[ChatTurn]) -> Vec<&[ChatTurn]> {
    if turns.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut used = 0usize;
    for (i, turn) in turns.iter().enumerate() {
        let t = estimate_tokens(&turn.content) + 4;
        if i > start && used + t > CHUNK_TOKENS {
            chunks.push(&turns[start..i]);
            start = i;
            used = 0;
        }
        used += t;
    }
    chunks.push(&turns[start..]);
    chunks
}

const COMPACT_SYSTEM: &str = r###"You maintain a context file for one chat thread. You are not the Coordinator.

The file is markdown with stable lines. Prefer ids like [f-12].

You do not rewrite the whole file. You emit a JSON patch:
{"ops":[...]}

ops:
- create: only if the file is empty. text is a markdown file with headings Facts, Open, Invalid.
- add: append fact lines
- replace: old must be an exact unique snippet, new is the replacement
- invalidate: old is an exact unique snippet to mark dead
- rewrite_section: heading plus text, only if replace cannot match

Rules:
- Facts, numbers, dates, caveats, holes. No prose. No new facts.
- If something was unverified, it stays unverified until a patch changes it.
- Same language as the messages.
- JSON only.
"###;

async fn compact_chunk(
    llm: &Llm,
    file: &str,
    overflow: &[ChatTurn],
) -> Result<String, String> {
    let empty = file.trim().is_empty();
    let user = if empty {
        format!(
            "The context file is empty. Create it from these messages (op create, then add if needed).\n\n{}",
            render_turns(overflow)
        )
    } else {
        format!(
            "Current context file:\n\n{}\n\n---\nAbsorb these older messages. Patch only. Do not repeat unchanged lines.\n\n{}",
            file,
            render_turns(overflow)
        )
    };
    let messages = vec![
        json!({"role": "system", "content": COMPACT_SYSTEM}),
        json!({"role": "user", "content": user}),
    ];
    let message = complete_tokens(llm, &messages, None, "none").await?;
    let raw = message_text(&message);
    if raw.is_empty() {
        return Err("Empty compact reply".into());
    }
    match parse_patch(&raw) {
        Ok(ops) => Ok(apply_ops(file, &ops)),
        Err(_) if empty => Ok(cap_summary(&raw)),
        Err(_) => {
            let mut id = next_id_for(file, "f");
            let mut out = file.to_string();
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            out.push_str("## Overflow\n");
            out.push_str(&with_ids(
                &format!("- compact parse failed; kept a stub\n"),
                &mut id,
                "f",
            ));
            Ok(cap_summary(&out))
        }
    }
}

async fn compact_file(llm: &Llm, file: &str, overflow: &[ChatTurn]) -> Result<String, String> {
    let mut summary = file.to_string();
    for chunk in chunk_turns(overflow) {
        summary = compact_chunk(llm, &summary, chunk).await?;
    }
    Ok(cap_summary(&summary))
}

pub struct Packed {
    pub summary: String,
    pub keep: Vec<ChatTurn>,
}

pub async fn pack_thread(
    app: &AppHandle,
    llm: &Llm,
    memory: &std::sync::Mutex<Memory>,
    thread_id: &str,
    turns: &[ChatTurn],
) -> Result<Packed, String> {
    let mut summary = {
        let g = memory
            .lock()
            .map_err(|_| "Context lock poisoned".to_string())?;
        g.summaries.get(thread_id).cloned()
    }
    .unwrap_or_else(|| load_summary_file(thread_id));

    if !needs_compact(&summary, turns) {
        return Ok(Packed {
            summary,
            keep: turns.to_vec(),
        });
    }

    let start = keep_start(turns);
    let overflow = &turns[..start];
    let keep = turns[start..].to_vec();
    if overflow.is_empty() {
        return Ok(Packed { summary, keep });
    }

    emit_pack(app, "pack", thread_id, None);
    let result = compact_file(llm, &summary, overflow).await;
    emit_pack(app, "packed", thread_id, None);
    summary = result?;

    {
        let mut g = memory
            .lock()
            .map_err(|_| "Context lock poisoned".to_string())?;
        g.summaries.insert(thread_id.to_string(), summary.clone());
    }
    save_summary(thread_id, &summary);

    Ok(Packed { summary, keep })
}

pub fn inject_summary(system: &str, summary: &str) -> String {
    let summary = summary.trim();
    if summary.is_empty() {
        return system.to_string();
    }
    format!("{system}\n\n## This thread\n\n{summary}")
}

#[tauri::command]
pub fn reset_context(
    app: AppHandle,
    memory: tauri::State<'_, std::sync::Mutex<Memory>>,
) -> Result<(), String> {
    app.state::<crate::ask::AskGate>().cancel();
    let mut g = memory
        .lock()
        .map_err(|_| "Context lock poisoned".to_string())?;
    g.summaries.clear();
    Ok(())
}

#[tauri::command]
pub fn load_ui_session() -> Result<Option<Value>, String> {
    let path = context_dir()?.join("ui.json");
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(|e| format!("Context ui: {e}"))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_str(&raw).map_err(|e| format!("Context ui: {e}"))?;
    Ok(Some(value))
}

#[tauri::command]
pub fn save_ui_session(session: Value) -> Result<(), String> {
    let path = context_dir()?.join("ui.json");
    let raw = serde_json::to_vec_pretty(&session).map_err(|e| format!("Context ui: {e}"))?;
    write_atomic(&path, &raw)
}

pub fn prune_for_api(messages: &[Value]) -> Vec<Value> {
    let budget = env_usize("PUCK_API_MAX_TOKENS", DEFAULT_API_MAX);
    let reserve = env_usize("PUCK_API_RESERVE_TOKENS", DEFAULT_API_RESERVE);
    let cap = budget.saturating_sub(reserve).max(8_000);
    let keep_max = env_usize("PUCK_PRUNE_KEEP_TURNS", DEFAULT_PRUNE_KEEP_TURNS).max(1);

    let imaged = prune_old_images(messages, 2);
    if messages_tokens(&imaged) <= cap {
        return imaged;
    }

    for keep in (1..=keep_max).rev() {
        let pruned = prune_tool_observations(&imaged, keep, 400);
        if messages_tokens(&pruned) <= cap || keep == 1 {
            return pruned;
        }
    }
    imaged
}

pub fn prune_for_model(messages: &[Value], sees_images: bool) -> Vec<Value> {
    let pruned = prune_for_api(messages);
    if sees_images {
        pruned
    } else {
        strip_image_urls(&pruned)
    }
}

pub fn strip_image_urls(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            if !has_image_url(msg) {
                return msg.clone();
            }
            let parts = msg
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let n = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("image_url"))
                .count();
            let text = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            let note = format!("[{n} images omitted — this model cannot see pixels.]");
            let body = if text.trim().is_empty() {
                note
            } else {
                format!("{text}\n{note}")
            };
            json!({ "role": "user", "content": body })
        })
        .collect()
}

fn messages_tokens(messages: &[Value]) -> usize {
    fn walk(v: &Value) -> usize {
        match v {
            Value::String(s) => estimate_tokens(s) + 1,
            Value::Array(a) => a.iter().map(walk).sum::<usize>().saturating_add(2),
            Value::Object(m) => m.values().map(walk).sum::<usize>().saturating_add(4),
            Value::Number(_) | Value::Bool(_) => 1,
            Value::Null => 0,
        }
    }
    messages.iter().map(walk).sum()
}

fn tool_call_meta(messages: &[Value]) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in calls {
            let id = call.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("");
            map.insert(id.to_string(), (name.to_string(), args_preview(args)));
        }
    }
    map
}

fn args_preview(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        for key in ["path", "pattern", "query", "command", "from", "to"] {
            if let Some(s) = v.get(key).and_then(Value::as_str) {
                let t = s.trim();
                if !t.is_empty() {
                    return clip_chars(t, 80);
                }
            }
        }
    }
    clip_chars(raw, 80)
}

fn clip_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn observation_head(content: &str) -> String {
    let mut out = String::new();
    for (i, line) in content.lines().enumerate() {
        if i >= STUB_HEAD_LINES {
            break;
        }
        let next = if out.is_empty() {
            line.to_string()
        } else {
            format!("{out}\n{line}")
        };
        if next.len() > STUB_HEAD_CHARS {
            if out.is_empty() {
                return clip_chars(line, STUB_HEAD_CHARS);
            }
            break;
        }
        out = next;
    }
    out
}

fn observation_stub(name: &str, args: &str, content: &str) -> String {
    let label = if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {args}")
    };
    let lines = content.lines().count();
    if content.starts_with("Error:") || content.starts_with("error:") {
        let head = clip_chars(content, 220);
        return format!("[{label} error]\n{head}");
    }
    let head = observation_head(content);
    format!(
        "{head}\n… [{label}: {lines} lines, older output cleared. Do not repeat this call unless the files changed.]"
    )
}

pub fn prune_tool_observations(
    messages: &[Value],
    keep_recent_tool_turns: usize,
    char_threshold: usize,
) -> Vec<Value> {
    if messages.is_empty() {
        return Vec::new();
    }

    let meta = tool_call_meta(messages);
    let mut tool_turn_ids: Vec<usize> = Vec::new();
    let mut in_tool_block = false;
    let mut current_turn_idx = 0;

    for (i, msg) in messages.iter().enumerate().rev() {
        let is_tool = msg.get("role").and_then(Value::as_str) == Some("tool");
        if is_tool {
            if !in_tool_block {
                in_tool_block = true;
                current_turn_idx += 1;
            }
            if current_turn_idx > keep_recent_tool_turns {
                tool_turn_ids.push(i);
            }
        } else {
            in_tool_block = false;
        }
    }

    let prune_set: std::collections::HashSet<usize> = tool_turn_ids.into_iter().collect();

    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            if prune_set.contains(&i) {
                if let Some(content) = msg.get("content").and_then(Value::as_str) {
                    if content.len() > char_threshold {
                        let id = msg
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let (name, args) = meta
                            .get(id)
                            .cloned()
                            .unwrap_or_else(|| ("tool".into(), String::new()));
                        let mut pruned_msg = msg.clone();
                        if let Value::Object(ref mut map) = pruned_msg {
                            map.insert(
                                "content".to_string(),
                                Value::String(observation_stub(&name, &args, content)),
                            );
                        }
                        return pruned_msg;
                    }
                }
            }
            msg.clone()
        })
        .collect()
}

fn has_image_url(msg: &Value) -> bool {
    msg.get("role").and_then(Value::as_str) == Some("user")
        && msg.get("content").and_then(Value::as_array).is_some_and(|parts| {
            parts
                .iter()
                .any(|p| p.get("type").and_then(Value::as_str) == Some("image_url"))
        })
}

pub fn prune_old_images(messages: &[Value], keep_recent: usize) -> Vec<Value> {
    let image_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| has_image_url(m))
        .map(|(i, _)| i)
        .collect();
    if image_idxs.len() <= keep_recent {
        return messages.to_vec();
    }
    let drop_n = image_idxs.len() - keep_recent;
    let drop: std::collections::HashSet<usize> = image_idxs.into_iter().take(drop_n).collect();
    messages
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            if !drop.contains(&i) {
                return msg.clone();
            }
            let parts = msg
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let text = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            let n = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("image_url"))
                .count();
            let note = format!(
                "[{n} screenshots omitted — already viewed earlier this job. Do not call view_page or view_image again unless you changed the page or need a file you have not seen.]"
            );
            let body = if text.trim().is_empty() {
                note
            } else {
                format!("{text}\n{note}")
            };
            json!({ "role": "user", "content": body })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_summary_keeps_standing_then_thread() {
        let out = inject_summary("standing orders", "yesterday");
        assert!(out.starts_with("standing orders"));
        assert!(out.contains("## This thread"));
        assert!(out.contains("yesterday"));
        assert!(!out.contains("Common memory"));
        assert!(!out.contains("Todos"));
    }

    fn turn(role: &str, content: &str) -> ChatTurn {
        ChatTurn {
            role: role.into(),
            content: content.into(),
        }
    }

    #[test]
    fn tokens_are_conservative() {
        assert!(estimate_tokens("ciao") >= 1);
        assert_eq!(estimate_tokens("abcdefghij"), 4);
    }

    #[test]
    fn keep_last_messages() {
        let turns: Vec<_> = (0..30)
            .map(|i| turn("user", &format!("m{i} xxxx")))
            .collect();
        let start = keep_start(&turns);
        assert!(start < turns.len());
        assert!(turns.len() - start <= 40);
        assert!(turns.len() - start >= 1);
    }

    #[test]
    fn replace_unique_and_skip_ambiguous() {
        let file = "# Context\n\n## Facts\n- [f-1] nord\n- [f-2] sud\n";
        let ops = vec![
            PatchOp {
                op: "replace".into(),
                old: Some("- [f-1] nord".into()),
                new: Some("- [f-1] nord, missing live".into()),
                text: None,
                heading: None,
            id: None,
            },
            PatchOp {
                op: "invalidate".into(),
                old: Some("- [f-2] sud".into()),
                new: None,
                text: None,
                heading: None,
            id: None,
            },
        ];
        let out = apply_ops(file, &ops);
        assert!(out.contains("- [f-1] nord, missing live"));
        assert!(out.contains("~~- [f-2] sud~~"));
    }

    #[test]
    fn add_assigns_ids() {
        let ops = vec![PatchOp {
            op: "add".into(),
            old: None,
            new: None,
            text: Some("- milano max unknown\n".into()),
            heading: None,
            id: None,
        }];
        let out = apply_ops("", &ops);
        assert!(out.contains("[f-1]"));
        assert!(out.contains("milano"));
    }

    #[test]
    fn absorb_keeps_disk_when_screen_is_cleared() {
        let disk = vec![
            turn("user", "nord"),
            turn("assistant", "ok nord"),
        ];
        let incoming = vec![turn("user", "sud")];
        let out = absorb(disk, incoming);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].content, "sud");
    }

    #[test]
    fn absorb_prefers_live_window() {
        let disk = vec![turn("user", "nord")];
        let incoming = vec![
            turn("user", "nord"),
            turn("assistant", "ok nord"),
            turn("user", "sud"),
        ];
        let out = absorb(disk, incoming);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].content, "sud");
    }

    #[test]
    fn comune_anyone_edits_the_whole_file() {
        let file = "# Common\n\nThe shop is a pharmacy in Carate.\nCopy: headline pending.\n";
        let ops = vec![PatchOp {
            op: "replace".into(),
            old: Some("Copy: headline pending.".into()),
            new: Some("Copy: Farmacia aperta 8–20.".into()),
            text: None,
            heading: None,
            id: None,
        }];
        let ok = apply_comune_to(file, &ops).unwrap();
        assert!(ok.contains("Farmacia aperta 8–20."));
        assert!(!ok.contains("headline pending"));
    }

    #[test]
    fn comune_delete_removes_stale() {
        let file = "# Common\n\nKeep this.\nDrop this.\n";
        let ops = vec![PatchOp {
            op: "delete".into(),
            old: Some("Drop this.\n".into()),
            new: None,
            text: None,
            heading: None,
            id: None,
        }];
        let ok = apply_comune_to(file, &ops).unwrap();
        assert!(ok.contains("Keep this."));
        assert!(!ok.contains("Drop this."));
    }

    #[test]
    fn comune_add_is_accepted_over_old_cap() {
        let file = "a".repeat(30);
        let ops = vec![PatchOp {
            op: "add".into(),
            old: None,
            new: None,
            text: Some("b".repeat(30)),
            heading: None,
            id: None,
        }];
        let ok = apply_comune_to(&file, &ops).unwrap();
        assert!(ok.contains('a'));
        assert!(ok.contains('b'));
    }

    #[test]
    fn memory_ops_by_id() {
        let file = "# Project\n\n## What this is\n- [m-1] A farm in Monza.\n- [m-2] Old line.\n";
        let add = vec![PatchOp {
            op: "add".into(),
            old: None,
            new: None,
            text: Some("New line.".into()),
            heading: None,
            id: Some("m-3".into()),
        }];
        let out = apply_comune_to(file, &add).unwrap();
        assert!(out.contains("- [m-3] New line."));
        let replace = vec![PatchOp {
            op: "replace".into(),
            old: None,
            new: Some("A flower shop in Vimercate.".into()),
            text: None,
            heading: None,
            id: Some("m-1".into()),
        }];
        let out = apply_comune_to(&out, &replace).unwrap();
        assert!(out.contains("- [m-1] A flower shop in Vimercate."));
        assert!(!out.contains("A farm in Monza"));
        let remove = vec![PatchOp {
            op: "remove".into(),
            old: None,
            new: None,
            text: None,
            heading: None,
            id: Some("m-2".into()),
        }];
        let out = apply_comune_to(&out, &remove).unwrap();
        assert!(!out.contains("Old line."));
        assert!(!out.contains("[m-2]"));
    }

    #[test]
    fn memory_add_gets_auto_id() {
        let file = "# Project\n\n## What this is\n- [m-1] X\n";
        let add = vec![PatchOp {
            op: "add".into(),
            old: None,
            new: None,
            text: Some("- Y\n".into()),
            heading: None,
            id: None,
        }];
        let out = apply_comune_to(file, &add).unwrap();
        assert!(out.contains("- [m-2] Y"));
    }

    #[test]
    fn cap_tokens_stays_under_max() {
        let file = (0..80)
            .map(|i| format!("line-{i} xxxx"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = cap_tokens(&file, 20);
        assert!(estimate_tokens(&out) <= 20);
        assert!(out.contains("line-0"));
    }

    #[test]
    fn chunk_markdown_splits() {
        let file = (0..40)
            .map(|i| format!("para {i} {}", "xxxx "))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_markdown(&file, 30);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks.join("\n"), file);
    }

    #[test]
    fn prune_tool_observations_masks_old_and_keeps_recent() {
        let long_blob = "x".repeat(500);
        let error_blob = format!("Error: command failed because {}", "y".repeat(400));
        let messages = vec![
            json!({"role": "system", "content": "You are a coder"}),
            json!({"role": "user", "content": "Build the page"}),
            // Turn 3 (oldest tool turn): 2 tool calls
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "function": {"name": "read_file"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": long_blob}),
            // Turn 2 (middle tool turn): error tool call
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c2", "function": {"name": "run"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": error_blob}),
            // Turn 1 (most recent tool turn): 1 short and 1 long tool call
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c3", "function": {"name": "patch_file"}}]}),
            json!({"role": "tool", "tool_call_id": "c3", "content": "File patched ok"}),
            json!({"role": "tool", "tool_call_id": "c4", "content": long_blob}),
        ];

        // Keep last 1 tool turn (c3, c4). Older turns (c1, c2) should be stubbed if > 300 chars.
        let pruned = prune_tool_observations(&messages, 1, 300);
        assert_eq!(pruned.len(), messages.len());

        assert_eq!(pruned[0]["content"], "You are a coder");
        assert_eq!(pruned[1]["content"], "Build the page");

        let c1_content = pruned[3]["content"].as_str().unwrap();
        assert!(c1_content.contains("older output cleared"), "{c1_content}");
        assert!(c1_content.contains("read_file"), "{c1_content}");
        assert!(c1_content.contains('x'), "{c1_content}");
        assert_eq!(pruned[3]["tool_call_id"], "c1");

        let c2_content = pruned[5]["content"].as_str().unwrap();
        assert!(c2_content.contains("error"), "{c2_content}");
        assert!(c2_content.contains("run"), "{c2_content}");
        assert_eq!(pruned[5]["tool_call_id"], "c2");

        assert_eq!(pruned[7]["content"], "File patched ok");
        assert_eq!(pruned[8]["content"], long_blob);
    }

    #[test]
    fn prune_for_api_keeps_tools_when_under_budget() {
        let blob = "hello workspace\nindex.html\nstyle.css\n";
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "build"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c1", "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": blob}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c2", "function": {"name": "read_file", "arguments": "{\"path\":\"index.html\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "<html>ok</html>"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c3", "function": {"name": "write_file"}}]}),
            json!({"role": "tool", "tool_call_id": "c3", "content": "Wrote: index.html. The next read sees this version."}),
        ];
        let pruned = prune_for_api(&messages);
        assert_eq!(pruned[3]["content"], blob);
        assert_eq!(pruned[5]["content"], "<html>ok</html>");
    }

    #[test]
    fn prune_old_images_keeps_recent() {
        fn img(label: &str) -> Value {
            json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": label },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,xx" } }
                ]
            })
        }
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            img("one"),
            json!({"role": "assistant", "content": "ok"}),
            img("two"),
            img("three"),
        ];
        let pruned = prune_old_images(&messages, 2);
        let one = pruned[1]["content"].as_str().unwrap();
        assert!(one.contains("omitted"), "{one}");
        assert!(one.contains("one"), "{one}");
        assert!(!one.contains("call view_image"), "{one}");
        assert!(!one.contains("call view_page again"), "{one}");
        assert!(pruned[3]["content"].is_array());
        assert!(pruned[4]["content"].is_array());
    }

    #[test]
    fn strip_image_urls_drops_pixels() {
        let messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "Look at these." },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,xx" } }
                ]
            }),
        ];
        let stripped = strip_image_urls(&messages);
        assert_eq!(stripped[0]["content"], "sys");
        let body = stripped[1]["content"].as_str().unwrap();
        assert!(body.contains("Look at these."));
        assert!(body.contains("cannot see pixels"));
        assert!(!body.contains("image_url"));
        let kept = prune_for_model(&messages, true);
        assert!(kept[1]["content"].is_array());
        let gone = prune_for_model(&messages, false);
        assert!(gone[1]["content"].as_str().unwrap().contains("cannot see pixels"));
    }

    #[test]
    fn working_folder_blurb_names_the_path() {
        let out = working_folder_blurb();
        assert!(out.contains("## Working folder"), "{out}");
        if out.contains("No project is open") {
            assert!(out.contains("open_project"), "{out}");
            assert!(out.contains("look_project"), "{out}");
        } else if out.contains("outside the vault") {
            assert!(out.contains("move work into the vault"), "{out}");
        } else {
            assert!(out.contains("open project"), "{out}");
            assert!(out.contains(".puck/memory.md"), "{out}");
        }
    }

    #[test]
    fn computer_root_is_slash() {
        assert!(is_computer_root(Path::new("/")));
        assert!(is_computer_root(&computer_root()));
        assert!(!is_computer_root(Path::new("/Users/me")));
        assert!(!is_computer_root(Path::new("/Users/me/sandbox")));
        let info = info_of(Path::new("/"), "default", true);
        assert!(!info.chosen);
        assert_eq!(info.path, "/");
    }

    #[test]
    fn comune_template_has_owner_and_workspaces() {
        assert!(COMUNE_TEMPLATE.contains("# User"));
        assert!(COMUNE_TEMPLATE.contains("## Workspaces"));
        let added =
            ensure_workspaces_heading("# User\n\nMattia likes short lines.\n").unwrap();
        assert!(added.contains("## Workspaces"));
        assert!(ensure_workspaces_heading(&added).is_none());
        let migrated = migrate_user_heading("# Owner\n\nFacts about the Owner, short.\n").unwrap();
        assert!(migrated.starts_with("# User\n"));
        assert!(migrated.contains("Facts about the User"));
        assert!(migrate_user_heading(&migrated).is_none());
    }

    #[test]
    fn project_template_is_not_ready() {
        assert!(project_picture_ok(PROJECT_TEMPLATE).is_err());
    }

    #[test]
    fn project_picture_needs_all_four_sections() {
        let thin = "# Project\n\nTwo lines.\nThat is all.\n";
        assert!(project_picture_ok(thin).is_err());
        let ok = "# Project\n\n## Structure\n- index.html — one-page vetrina for a local shop, hero plus contacts\n- images/hero.webp — photo of the shopfront\n\n## What this is\nA one-page site for Sito Giusto, a small shop. No CMS, no prices on the page, WhatsApp CTA.\n\n## Done\nindex.html exists and renders a hero, a short about, and a tel link. No 390 euro figure anywhere in the files.\n\n## Missing\nNo real phone number in the HTML. Hours are placeholders. Need User facts before print.\n";
        assert!(project_picture_ok(ok).is_ok(), "{:?}", project_picture_ok(ok));
        let rewritten = apply_comune_to(
            PROJECT_TEMPLATE,
            &[PatchOp {
                op: "rewrite".into(),
                old: None,
                new: None,
                text: Some(ok.to_string()),
                heading: None,
            id: None,
            }],
        )
        .unwrap();
        assert!(project_picture_ok(&rewritten).is_ok());
        assert!(!rewritten.contains("Realign on first look"));
        let create_err = apply_comune_to(
            PROJECT_TEMPLATE,
            &[PatchOp {
                op: "create".into(),
                old: None,
                new: None,
                text: Some(ok.to_string()),
                heading: None,
            id: None,
            }],
        )
        .unwrap_err();
        assert!(create_err.contains("rewrite"), "{create_err}");
        let added = apply_comune_to(
            PROJECT_TEMPLATE,
            &[PatchOp {
                op: "add".into(),
                old: None,
                new: None,
                text: Some("A stray note without headings.\n".into()),
                heading: None,
            id: None,
            }],
        )
        .unwrap();
        assert!(project_picture_ok(&added).is_err());
    }
}
