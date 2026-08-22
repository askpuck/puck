use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

pub const LIVE_MARK: &str = "## Live context";

pub const PLAN_NUDGE: &str = "Call todo_write first with the steps for this job, in the order you will do them. One in_progress: the first. Then do that step. Do not skip this.";

pub const FINISH_NUDGE: &str = "The list still has steps. Do the in_progress item. When it is done, call todo_write again without it (drop finished steps). Put the next in in_progress. Add a step only if this brief needs it. Do not add a review of sibling files this brief did not name. Omit steps that are no longer useful. Do not recap while the list has items.";

#[derive(Clone, Serialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

pub fn tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "todo_write",
            "description": "Replace this job's remaining todo list. Call it first, before other tools, with the steps in the order you will do them. One in_progress: the current step. When a step is done, call again without it — drop finished steps, do not keep them as done. Add a step only if this brief needs it. Do not add a review of sibling files this brief did not name. When the list is empty, the job's steps are done: recap and stop.",
            "parameters": {
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "description": "pending or in_progress. Omit a finished step — do not keep it as done. status done also drops the item."
                                }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["items"]
            }
        }
    })
}

pub fn text(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    items
        .iter()
        .map(|t| format!("- [{}] {}", t.status, t.content))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn with_todos(body: &str, items: &[TodoItem]) -> String {
    if items.is_empty() {
        return body.to_string();
    }
    format!(
        "{body}\n\n## Todos\nRemaining steps for this job, in order. One in_progress: do that now. When a step is done, call todo_write without it. Add a step only if this brief needs it. Empty list = the steps are done.\n\n{}",
        text(items)
    )
}

fn todos_section(items: &[TodoItem]) -> String {
    with_todos("", items).trim_start().to_string()
}

pub fn is_live_message(msg: &Value) -> bool {
    msg.get("role").and_then(Value::as_str) == Some("user")
        && msg
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with(LIVE_MARK))
}

pub fn live_text(comune: &str, project: &str, awake: Option<&str>, items: &[TodoItem]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let comune = comune.trim();
    if !comune.is_empty() {
        parts.push(format!(
            "## User memory\n\nFacts about the User: who they are, what they run, their hours, how they work, standing preferences. Survives folders. If a fact about them leaked this job, call patch_comune. A live number about the world is not them. Workspace notes: folder name + full path, only for real work — not a one-off question in an empty folder. Not a diary.\n\n{comune}"
        ));
    }
    let folder = crate::context::working_folder_blurb();
    if !folder.is_empty() {
        parts.push(folder);
    }
    let project = project.trim();
    if !project.is_empty() {
        parts.push(format!(
            "## This project\n\nThis folder's memory (.puck/memory.md). Obligatory headings: ## Structure, ## What this is, ## Done, ## Missing. After a first look or a realign, call patch_project with op=rewrite and text=the whole file. Not two thin lines. Not User prefs.\n\n{project}"
        ));
    }
    if let Some(awake) = awake.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("## Currently awake\n\n{awake}"));
    }
    if !items.is_empty() {
        parts.push(todos_section(items));
    }
    parts.join("\n\n")
}

pub fn pin_live_with(
    messages: &mut Vec<Value>,
    comune: &str,
    project: &str,
    awake: Option<&str>,
    items: &[TodoItem],
) {
    messages.retain(|m| !is_live_message(m));
    let body = live_text(comune, project, awake, items);
    if body.is_empty() {
        return;
    }
    messages.push(json!({
        "role": "user",
        "content": format!(
            "{LIVE_MARK}\nBackground state for this call, sent last. It does not override a more recent instruction above it.\n\n{body}"
        )
    }));
}

pub fn pin_live(messages: &mut Vec<Value>, awake: Option<&str>, items: &[TodoItem]) {
    pin_live_with(
        messages,
        &crate::context::load_comune(),
        &crate::context::load_project_now(),
        awake,
        items,
    );
}

pub fn all_done(items: &[TodoItem]) -> bool {
    items.iter().all(|t| t.status == "done")
}

pub fn still_open(items: &[TodoItem]) -> bool {
    !all_done(items)
}

pub fn record_plan(planned: &mut bool, items: &[TodoItem]) {
    if !items.is_empty() {
        *planned = true;
    }
}

pub fn needs_plan(planned: bool) -> bool {
    !planned
}

pub fn needs_finish(planned: bool, items: &[TodoItem]) -> bool {
    planned && still_open(items)
}

pub fn parse(args: &Value) -> Result<Vec<TodoItem>, String> {
    let items = args
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "Missing items.".to_string())?;
    if items.len() > 12 {
        return Err("At most 12 todos.".into());
    }
    let mut out = Vec::new();
    let mut saw_progress = false;
    for it in items {
        let content = it
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            continue;
        }
        let raw = it
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .trim()
            .to_ascii_lowercase();
        let mut status = match raw.as_str() {
            "in_progress" | "in-progress" | "doing" => "in_progress",
            "done" | "completed" => "done",
            _ => "pending",
        };
        if status == "done" {
            continue;
        }
        if status == "in_progress" {
            if saw_progress {
                status = "pending";
            } else {
                saw_progress = true;
            }
        }
        out.push(TodoItem {
            content: content.to_string(),
            status: status.into(),
        });
    }
    if !saw_progress {
        if let Some(first) = out.first_mut() {
            first.status = "in_progress".into();
        }
    }
    Ok(out)
}

pub fn apply(todos: &mut Vec<TodoItem>, args: &str, app: &AppHandle, role: &str) -> String {
    let value: Value = serde_json::from_str(args).unwrap_or(json!({}));
    match parse(&value) {
        Ok(items) => {
            *todos = items;
            emit(app, role, todos);
            if todos.is_empty() {
                "Remaining list is empty. If this job's steps are done, recap and stop. If you still have work, send the remaining steps."
                    .into()
            } else {
                format!("Todos:\n{}", text(todos))
            }
        }
        Err(e) => format!("Error: {e}"),
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Pulse {
    kind: &'static str,
    role: String,
    text: String,
}

fn emit(app: &AppHandle, role: &str, items: &[TodoItem]) {
    let text = serde_json::to_string(items).unwrap_or_else(|_| "[]".into());
    let _ = app.emit(
        "puck-crew",
        Pulse {
            kind: "todo",
            role: role.to_string(),
            text,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_keeps_order_and_one_in_progress() {
        let items = parse(&json!({
            "items": [
                {"content": "Name check", "status": "done"},
                {"content": "Street", "status": "in_progress"},
                {"content": "Transit", "status": "doing"},
                {"content": "", "status": "pending"},
                {"content": "CAP", "status": "pending"}
            ]
        }))
        .unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].content, "Street");
        assert_eq!(items[0].status, "in_progress");
        assert_eq!(items[1].status, "pending");
        assert_eq!(items[2].content, "CAP");
        assert!(still_open(&items));
        assert!(!all_done(&items));
    }

    #[test]
    fn parse_drops_done_and_promotes_first() {
        let items = parse(&json!({
            "items": [
                {"content": "Look", "status": "pending"},
                {"content": "Write", "status": "pending"}
            ]
        }))
        .unwrap();
        assert_eq!(items[0].status, "in_progress");
        assert_eq!(items[1].status, "pending");
        let gone = parse(&json!({
            "items": [{"content": "Look", "status": "done"}]
        }))
        .unwrap();
        assert!(gone.is_empty());
        assert!(all_done(&gone));
        assert!(!needs_finish(false, &gone));
        assert!(needs_finish(true, &items));
        assert!(needs_plan(false));
        assert!(!needs_plan(true));
    }

    #[test]
    fn parse_caps_and_all_done() {
        let too_many: Vec<Value> = (0..13)
            .map(|i| json!({"content": i.to_string(), "status": "pending"}))
            .collect();
        assert!(parse(&json!({ "items": too_many })).is_err());
        let done = parse(&json!({
            "items": [
                {"content": "A", "status": "completed"},
                {"content": "B", "status": "done"}
            ]
        }))
        .unwrap();
        assert!(done.is_empty());
        assert!(all_done(&done));
        assert_eq!(with_todos("hi", &done), "hi");
        assert_eq!(with_todos("hi", &[]), "hi");
    }

    #[test]
    fn pin_live_puts_variable_state_last_and_replaces() {
        let mut messages = vec![
            json!({"role": "system", "content": "standing"}),
            json!({"role": "user", "content": "build"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        let todos = parse(&json!({
            "items": [{"content": "A", "status": "in_progress"}]
        }))
        .unwrap();
        pin_live_with(&mut messages, "shop facts", "this folder", Some("coder"), &todos);
        assert_eq!(messages[0]["content"], "standing");
        let last = messages.last().unwrap()["content"].as_str().unwrap();
        assert!(last.starts_with(LIVE_MARK));
        assert!(last.contains("shop facts"));
        assert!(last.contains("Currently awake"));
        assert!(last.contains("coder"));
        assert!(last.contains("in_progress"));
        pin_live_with(
            &mut messages,
            "shop facts",
            "this folder",
            Some("coder"),
            &todos,
        );
        assert_eq!(
            messages.iter().filter(|m| is_live_message(m)).count(),
            1
        );
        assert_eq!(messages.len(), 4);
        assert!(messages.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("coder"));
    }
}
