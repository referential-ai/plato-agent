use crate::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 1_024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicMessage {
    pub role: AnthropicRole,
    pub content: AnthropicContent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicBlock>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AnthropicBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AnthropicResponse {
    pub content: Vec<AnthropicBlock>,
    pub usage: AnthropicUsage,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: &'a [AnthropicMessage],
    tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Serialize)]
struct ToolDefinition {
    name: String,
    description: String,
    input_schema: Value,
}

pub struct AnthropicClient {
    api_key: String,
}

impl AnthropicClient {
    pub fn from_env(api_key_env: &str) -> AppResult<Self> {
        let api_key =
            std::env::var(api_key_env).map_err(|_| AppError::MissingApiKey(api_key_env.into()))?;
        Ok(Self { api_key })
    }

    pub fn send(
        &self,
        model: &str,
        system: &str,
        messages: &[AnthropicMessage],
        enabled_tools: &[String],
    ) -> AppResult<AnthropicResponse> {
        let request = AnthropicRequest {
            model,
            max_tokens: DEFAULT_MAX_TOKENS,
            system,
            messages,
            tools: tool_definitions(enabled_tools),
        };

        let response = ureq::post(MESSAGES_URL)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", ANTHROPIC_VERSION)
            .set("content-type", "application/json")
            .send_json(request);

        match response {
            Ok(response) => response
                .into_json::<AnthropicResponse>()
                .map_err(|error| AppError::Provider(error.to_string())),
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(AppError::Provider(format!(
                    "anthropic returned http {status}: {body}"
                )))
            }
            Err(error) => Err(AppError::Provider(error.to_string())),
        }
    }
}

impl AnthropicMessage {
    pub fn user_text(content: impl Into<String>) -> Self {
        Self {
            role: AnthropicRole::User,
            content: AnthropicContent::Text(content.into()),
        }
    }

    pub fn assistant_blocks(content: Vec<AnthropicBlock>) -> Self {
        Self {
            role: AnthropicRole::Assistant,
            content: AnthropicContent::Blocks(content),
        }
    }

    pub fn tool_result(tool_use_id: String, content: String, is_error: bool) -> Self {
        Self {
            role: AnthropicRole::User,
            content: AnthropicContent::Blocks(vec![AnthropicBlock::ToolResult {
                tool_use_id,
                content,
                is_error: is_error.then_some(true),
            }]),
        }
    }
}

impl AnthropicResponse {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                AnthropicBlock::Text { text } => Some(text.as_str()),
                AnthropicBlock::ToolUse { .. } | AnthropicBlock::ToolResult { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_uses(&self) -> Vec<(String, String, Value)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                AnthropicBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                AnthropicBlock::Text { .. } | AnthropicBlock::ToolResult { .. } => None,
            })
            .collect()
    }
}

fn tool_definitions(enabled_tools: &[String]) -> Vec<ToolDefinition> {
    enabled_tools
        .iter()
        .filter_map(|tool| match tool.as_str() {
            "file.read" => Some(ToolDefinition {
                name: "file.read".into(),
                description: "Read a UTF-8 text file inside the current workspace.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path inside the current workspace."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            }),
            "file.write" => Some(ToolDefinition {
                name: "file.write".into(),
                description: "Write UTF-8 text to a relative path inside the current workspace after approval.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path inside the current workspace."
                        },
                        "content": {
                            "type": "string",
                            "description": "UTF-8 content to write."
                        }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            }),
            _ => None,
        })
        .collect()
}

pub fn system_prompt() -> &'static str {
    "You are Plato Agent. Use at most one tool call in a response. Use file.read when you need to inspect workspace files. Use file.write only when the user explicitly asks you to write or edit a file. After a tool result, answer the user directly or request exactly one next tool call."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_and_tool_uses_from_response() {
        let response: AnthropicResponse = serde_json::from_value(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-5",
            "stop_reason": "tool_use",
            "content": [
                {
                    "type": "text",
                    "text": "I will read it."
                },
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "file.read",
                    "input": {"path": "README.md"}
                }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        }))
        .unwrap();

        assert_eq!(response.text(), "I will read it.");
        assert_eq!(response.tool_uses().len(), 1);
    }
}
