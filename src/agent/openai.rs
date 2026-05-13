//! OpenAI-compatible chat-completions backend.
//!
//! Most modern LLM providers speak this exact dialect. Works with:
//!   - OpenAI (the canonical implementation)
//!   - OpenRouter (`https://openrouter.ai/api/v1`, any model)
//!   - Groq, Mistral, DeepSeek, xAI/Grok, Together
//!   - Local servers: Ollama (`http://localhost:11434/v1`),
//!     LM Studio (`http://localhost:1234/v1`), vLLM
//!
//! The backend runs an iteration loop: the model can call tools (see
//! [`crate::agent::tools`]), each tool result is appended to the message
//! history, and the loop continues until the model emits a final assistant
//! message with no further tool calls. That final message must be a JSON
//! object matching the `StructuredIntent` shape; the caller parses it.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::intent::StructuredIntent;
use crate::agent::llm::parse_intent_json;
use crate::agent::tools;

/// Caps the agent loop. Each round-trip costs a real LLM call, so we'd rather
/// fail loudly than spend a wallet of tokens on a stuck conversation.
pub const MAX_ITERATIONS: usize = 6;

/// All the knobs that distinguish "OpenAI" from "OpenRouter" from "Ollama".
#[derive(Debug, Clone)]
pub struct OpenAICompatConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl OpenAICompatConfig {
    /// Resolve a config from the CLI flag overrides + env-var fallbacks.
    /// Precedence: explicit flag value > env var > default.
    pub fn resolve(
        base_url: Option<&str>,
        model: Option<&str>,
        api_key_env: Option<&str>,
    ) -> Result<Self> {
        let base_url = base_url
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".into());

        let model = model
            .map(|s| s.to_string())
            .or_else(|| std::env::var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| "gpt-4o-mini".into());

        // API key precedence:
        //   1. --api-key-env <VAR> reads from that named env var
        //   2. OPENAI_API_KEY (the canonical name)
        let api_key = if let Some(var) = api_key_env {
            std::env::var(var).with_context(|| format!("env var `{var}` is unset"))?
        } else {
            std::env::var("OPENAI_API_KEY")
                .context("OPENAI_API_KEY is unset (or pass --api-key-env <VAR>)")?
        };

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
        })
    }
}

/* ───────────────────────────────────────────────── chat-completions message types */

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum ChatMessage {
    System {
        role: &'static str,
        content: String,
    },
    User {
        role: &'static str,
        content: String,
    },
    Assistant {
        role: &'static str,
        content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        role: &'static str,
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: ToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCallFunction {
    name: String,
    /// `arguments` from OpenAI is a JSON-encoded string, not an object.
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

/* ───────────────────────────────────────────────────────────────── public entrypoint */

const SYSTEM_PROMPT: &str = include_str!("system_prompt.txt");

pub async fn resolve(prompt: &str, cfg: OpenAICompatConfig) -> Result<StructuredIntent> {
    let mut messages: Vec<ChatMessage> = vec![
        ChatMessage::System {
            role: "system",
            content: SYSTEM_PROMPT.into(),
        },
        ChatMessage::User {
            role: "user",
            content: prompt.into(),
        },
    ];
    let tool_defs = tools::definitions();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let url = format!("{}/chat/completions", cfg.base_url);

    for iteration in 0..MAX_ITERATIONS {
        let body = json!({
            "model": cfg.model,
            "messages": messages,
            "tools": tool_defs,
            "tool_choice": "auto",
            "temperature": 0.2,
        });

        let response = client
            .post(&url)
            .bearer_auth(&cfg.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            bail!("LLM provider returned {status}: {text}");
        }

        let chat: ChatResponse = response
            .json()
            .await
            .context("parsing chat-completions response")?;

        let choice = chat
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("LLM returned no choices"))?;
        let msg = choice.message;

        // If the model called tools, execute them and continue the loop.
        if !msg.tool_calls.is_empty() {
            messages.push(ChatMessage::Assistant {
                role: "assistant",
                content: msg.content.clone(),
                tool_calls: msg.tool_calls.clone(),
            });
            for call in msg.tool_calls {
                let args_value: Value =
                    serde_json::from_str(&call.function.arguments).unwrap_or(json!({}));
                let result = match tools::execute(&call.function.name, &args_value).await {
                    Ok(s) => s,
                    Err(e) => format!("{{\"error\":\"{e}\"}}"),
                };
                messages.push(ChatMessage::Tool {
                    role: "tool",
                    tool_call_id: call.id,
                    content: result,
                });
            }
            continue;
        }

        // No tool calls → expect a final JSON intent.
        let final_text = msg
            .content
            .ok_or_else(|| anyhow!("iteration {iteration}: model returned no content"))?;
        return parse_intent_json(&final_text);
    }

    bail!(
        "agent did not converge within {MAX_ITERATIONS} iterations; \
         the model kept calling tools without producing a final intent"
    );
}
