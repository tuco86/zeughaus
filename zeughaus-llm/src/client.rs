//! Minimal blocking client for LM Studio's OpenAI-compatible local API.
//!
//! LM Studio serves `POST /v1/chat/completions` and `GET /v1/models`. We talk
//! to it synchronously via ureq so node execution stays on the executor thread
//! like every other plugin. Base URL defaults to `http://localhost:1234/v1`.

use std::time::Instant;

use serde_json::{json, Value as Json};

use crate::conversation::{ChatMessage, Conversation, Role};

pub const DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";

/// Outcome of a chat completion: the assistant reply plus speed telemetry.
pub struct ChatResult {
    pub content: String,
    pub completion_tokens: u64,
    pub elapsed_secs: f64,
}

impl ChatResult {
    /// Generation throughput. Zero if the server reported no token count or
    /// the call returned instantly.
    pub fn tokens_per_sec(&self) -> f64 {
        if self.elapsed_secs > 0.0 {
            self.completion_tokens as f64 / self.elapsed_secs
        } else {
            0.0
        }
    }
}

fn join(base_url: &str, path: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), path)
}

/// Sends the conversation to the model and returns the assistant reply.
/// `model` may be empty: LM Studio then uses its currently loaded model.
pub fn chat_completion(
    base_url: &str,
    model: &str,
    conversation: &Conversation,
) -> Result<ChatResult, String> {
    let messages: Vec<Json> = conversation
        .messages
        .iter()
        .map(|m| json!({ "role": m.role.as_str(), "content": m.content }))
        .collect();

    let body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
    });

    let url = join(base_url, "chat/completions");
    let started = Instant::now();
    let response = ureq::post(&url)
        .send_json(body)
        .map_err(|e| describe_error("chat/completions", e))?;
    let elapsed_secs = started.elapsed().as_secs_f64();

    let parsed: Json = response
        .into_json()
        .map_err(|e| format!("invalid JSON from LM Studio: {e}"))?;

    let content = parsed["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| format!("unexpected response shape: {parsed}"))?
        .to_string();

    let completion_tokens = parsed["usage"]["completion_tokens"].as_u64().unwrap_or(0);

    Ok(ChatResult {
        content,
        completion_tokens,
        elapsed_secs,
    })
}

/// Lists model ids currently available in LM Studio.
pub fn list_models(base_url: &str) -> Result<Vec<String>, String> {
    let url = join(base_url, "models");
    let parsed: Json = ureq::get(&url)
        .call()
        .map_err(|e| describe_error("models", e))?
        .into_json()
        .map_err(|e| format!("invalid JSON from LM Studio: {e}"))?;

    let models = parsed["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok(models)
}

/// Convenience used by the chat node: append the model reply to the
/// conversation and return the extended conversation alongside telemetry.
pub fn chat_and_append(
    base_url: &str,
    model: &str,
    conversation: &Conversation,
) -> Result<(Conversation, ChatResult), String> {
    let result = chat_completion(base_url, model, conversation)?;
    let extended = conversation.with(ChatMessage::new(Role::Assistant, &result.content));
    Ok((extended, result))
}

fn describe_error(endpoint: &str, err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, _) => {
            format!("LM Studio {endpoint} returned HTTP {code}")
        }
        ureq::Error::Transport(t) => {
            format!("cannot reach LM Studio at {endpoint}: {t} (is the server running?)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_normalizes_trailing_slash() {
        assert_eq!(
            join("http://localhost:1234/v1/", "models"),
            "http://localhost:1234/v1/models"
        );
        assert_eq!(
            join("http://localhost:1234/v1", "chat/completions"),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    #[test]
    fn tokens_per_sec_handles_zero_elapsed() {
        let r = ChatResult {
            content: String::new(),
            completion_tokens: 10,
            elapsed_secs: 0.0,
        };
        assert_eq!(r.tokens_per_sec(), 0.0);
    }
}
