use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};

pub struct AskGate {
    tx: Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
}

#[derive(Clone, Serialize)]
struct AskPrompt {
    who: String,
    question: String,
}

impl AskGate {
    pub fn new() -> Self {
        Self {
            tx: Mutex::new(None),
        }
    }

    fn take_tx(&self) -> Option<tokio::sync::oneshot::Sender<String>> {
        self.tx.lock().ok()?.take()
    }

    pub fn answer(&self, text: String) -> Result<(), String> {
        let tx = self
            .take_tx()
            .ok_or_else(|| "No question is open.".to_string())?;
        tx.send(text)
            .map_err(|_| "The question is no longer waiting.".to_string())
    }

    pub fn cancel(&self) {
        if let Some(tx) = self.take_tx() {
            let _ = tx.send(String::new());
        }
    }
}

pub fn tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "ask_user",
            "description": "Ask the User one question. A box opens in the chat; the job waits until they answer. Use when the work cannot continue without something only they know (their hours, their number, which of two things they meant). This is not asking them to search. Do not use for facts the live web can give, for a yes on a file write, or to chat.",
            "parameters": {
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The prompt they see. One question. Their language. Short."
                    }
                },
                "required": ["question"]
            }
        }
    })
}

pub fn question_from_args(args: &str) -> String {
    serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|v| {
            v.get("question")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default()
}

pub async fn run(app: &AppHandle, who: &str, args: &str) -> String {
    let question = question_from_args(args);
    if question.is_empty() {
        return "Error: Missing question.".into();
    }
    let gate = app.state::<AskGate>();
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let mut slot = match gate.tx.lock() {
            Ok(g) => g,
            Err(_) => return "Error: Ask lock poisoned.".into(),
        };
        if slot.is_some() {
            return "A question is already open. Wait for the User.".into();
        }
        *slot = Some(tx);
    }
    crate::context::run_log(
        "ask_user",
        json!({
            "who": who,
            "question": crate::context::clip_log(&question, 160),
        }),
    );
    let _ = app.emit(
        "puck-ask",
        AskPrompt {
            who: who.to_string(),
            question: question.clone(),
        },
    );
    match rx.await {
        Ok(answer) => wrap_reply(&answer),
        Err(_) => wrap_reply(""),
    }
}

pub fn wrap_reply(raw: &str) -> String {
    let answer = raw.trim();
    if answer.is_empty() {
        "The User closed the question without answering. This is not a new order. Stay on the current order.".into()
    } else {
        format!(
            "The User answered the question you just asked. This is not a new order and not a greeting. It is context for the order you were already executing. Resume that same order with this answer.\n\n{answer}"
        )
    }
}

#[tauri::command]
pub fn answer_user(gate: State<'_, AskGate>, text: String) -> Result<(), String> {
    gate.answer(text)
}

#[cfg(test)]
mod tests {
    use super::{question_from_args, wrap_reply};

    #[test]
    fn reads_question() {
        assert_eq!(
            question_from_args(r#"{"question":" Che orario? "}"#),
            "Che orario?"
        );
        assert!(question_from_args("{}").is_empty());
        assert!(question_from_args("nope").is_empty());
    }

    #[test]
    fn wrap_keeps_answer_as_context() {
        let out = wrap_reply(" Mattia ");
        assert!(out.contains("Mattia"));
        assert!(out.contains("not a new order"));
        assert!(wrap_reply("").contains("not a new order"));
    }
}
