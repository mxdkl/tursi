//! Provider-agnostic chat types and the provider adapters (§7.1).
//! Enum dispatch, no dyn: adapters are added as variants.

pub mod openai_compat;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    System(String),
    /// Also the vehicle for steering, `[verify]`, and mode injections (§8.4).
    User(String),
    Assistant { text: String, tool_calls: Vec<ToolCall> },
    ToolResult { call_id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Set when the streamed arguments weren't usable JSON: the executor
    /// answers with this instead of running the call.
    #[serde(skip)]
    pub malformed: Option<String>,
}

/// An HTTP error status from the provider. 408/429/5xx are worth retrying;
/// any other 4xx is a request the provider will keep rejecting.
#[derive(Debug)]
pub struct ApiError {
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl ApiError {
    pub fn retryable(&self) -> bool {
        let code = self.status.as_u16();
        code == 408 || code == 429 || self.status.is_server_error()
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "api error {}: {}", self.status, self.body)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// Stable order across a session — part of the cacheable prefix (§8.4).
    pub tools: Vec<ToolSchema>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
}

/// Streaming deltas for the TUI; the settled `Turn` is the API of record.
pub enum StreamEvent {
    TextDelta(String),
    /// Read once the UI shows in-flight tool composition during streaming;
    /// run_batch announces executed calls today.
    #[allow(dead_code)]
    ToolCallStarted { name: String },
}

/// One finished assistant turn.
pub struct Turn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

pub enum Provider {
    /// DeepSeek, Qwen, Kimi, OpenRouter, … — everything speaking the de-facto
    /// standard. New adapters become new variants.
    OpenAiCompat(openai_compat::Client),
}

impl Provider {
    /// Adapter for whichever provider serves `model`, using the base URL and
    /// key from `secrets.toml` (prefix-routed: "deepseek/…" → deepseek entry).
    pub fn for_model(model: &str, secrets: &crate::config::Secrets, config: &crate::config::Config) -> Result<Provider> {
        let (key, base_url) = secrets.provider_for(model).ok_or_else(|| {
            anyhow::anyhow!(
                "no credentials for {model} — add it to ~/.tursi/secrets.toml or set <PREFIX>_API_KEY"
            )
        })?;
        let client = openai_compat::Client::new(key, base_url).with_headers(secrets.headers_for(model));
        let client = match config.models.get(model) {
            Some(options) => client.with_options(options),
            None => client,
        };
        Ok(Provider::OpenAiCompat(client))
    }

    /// One streaming chat call; deltas go to `events`, the settled turn returns.
    pub async fn chat(&self, req: ChatRequest, events: mpsc::Sender<StreamEvent>) -> Result<Turn> {
        match self {
            Provider::OpenAiCompat(c) => c.chat(req, events).await,
        }
    }
}
