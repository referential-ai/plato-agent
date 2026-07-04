use crate::{AppError, AppResult};
use platonic_core::ModelUsage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRequest {
    pub model: String,
    pub system: String,
    pub max_output_tokens: u32,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: Vec<ModelBlock>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ModelBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStop {
    EndTurn,
    ToolUse,
    MaxOutput,
    ContentFilter,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResponse {
    pub content: Vec<ModelBlock>,
    pub stop: ModelStop,
    pub usage: ModelUsage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub struct OpenAiCompatibleClient {
    api_key: String,
    base_url: String,
    timeout: Duration,
    http_referer: Option<String>,
    app_title: Option<String>,
    token_limit_field: TokenLimitField,
}

impl OpenAiCompatibleClient {
    pub fn from_config(
        api_key_env: &str,
        base_url: String,
        timeout_ms: u64,
        http_referer: Option<String>,
        app_title: Option<String>,
        token_limit_field: TokenLimitField,
    ) -> AppResult<Self> {
        let api_key =
            std::env::var(api_key_env).map_err(|_| AppError::MissingApiKey(api_key_env.into()))?;
        if base_url.trim().is_empty() {
            return Err(AppError::Config(
                "provider.base_url must not be empty".into(),
            ));
        }
        if timeout_ms == 0 {
            return Err(AppError::Config(
                "provider.timeout_ms must be positive".into(),
            ));
        }
        Ok(Self {
            api_key,
            base_url,
            timeout: Duration::from_millis(timeout_ms),
            http_referer,
            app_title,
            token_limit_field,
        })
    }

    pub fn send(&self, request: &ModelRequest) -> AppResult<ModelResponse> {
        let body = ChatCompletionRequest::from_model_request(request, self.token_limit_field)?;
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let mut call = agent
            .post(&url)
            .set("authorization", &format!("Bearer {}", self.api_key))
            .set("content-type", "application/json");
        if let Some(http_referer) = &self.http_referer {
            call = call.set("HTTP-Referer", http_referer);
        }
        if let Some(app_title) = &self.app_title {
            call = call.set("X-OpenRouter-Title", app_title);
        }

        match call.send_json(body) {
            Ok(response) => response
                .into_json::<ChatCompletionResponse>()
                .map_err(|error| AppError::Provider(error.to_string()))?
                .into_model_response(),
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                Err(AppError::Provider(format!(
                    "provider returned http {status}: {body}"
                )))
            }
            Err(error) => Err(AppError::Provider(error.to_string())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenLimitField {
    MaxTokens,
    MaxCompletionTokens,
}

impl ModelMessage {
    pub fn user_text(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::User,
            content: vec![ModelBlock::Text {
                text: content.into(),
            }],
        }
    }

    pub fn assistant_blocks(content: Vec<ModelBlock>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content,
        }
    }

    pub fn tool_result(tool_call_id: String, content: String, is_error: bool) -> Self {
        Self {
            role: ModelRole::Tool,
            content: vec![ModelBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            }],
        }
    }
}

impl ModelResponse {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ModelBlock::Text { text } => Some(text.as_str()),
                ModelBlock::ToolUse { .. } | ModelBlock::ToolResult { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_uses(&self) -> Vec<(String, String, Value)> {
        self.content
            .iter()
            .filter_map(|block| match block {
                ModelBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                ModelBlock::Text { .. } | ModelBlock::ToolResult { .. } => None,
            })
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    tools: Vec<ChatTool>,
    tool_choice: &'static str,
    parallel_tool_calls: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatMessage {
    role: ChatRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatTool {
    #[serde(rename = "type")]
    tool_type: ChatToolType,
    function: ChatFunctionDefinition,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChatToolType {
    Function,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatFunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    tool_type: ChatToolType,
    function: ChatFunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    finish_reason: ChatFinishReason,
    message: ChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ChatToolCall>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChatFinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
}

#[derive(Clone, Debug, Deserialize)]
struct ChatUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

impl ChatCompletionRequest {
    fn from_model_request(
        request: &ModelRequest,
        token_limit_field: TokenLimitField,
    ) -> AppResult<Self> {
        let mut messages = Vec::with_capacity(request.messages.len() + 1);
        messages.push(ChatMessage {
            role: ChatRole::System,
            content: Some(request.system.clone()),
            tool_calls: None,
            tool_call_id: None,
        });
        for message in &request.messages {
            messages.push(ChatMessage::from_model_message(message)?);
        }

        Ok(Self {
            model: request.model.clone(),
            messages,
            tools: request.tools.iter().map(ChatTool::from_tool_spec).collect(),
            tool_choice: "auto",
            parallel_tool_calls: false,
            max_tokens: matches!(token_limit_field, TokenLimitField::MaxTokens)
                .then_some(request.max_output_tokens),
            max_completion_tokens: matches!(
                token_limit_field,
                TokenLimitField::MaxCompletionTokens
            )
            .then_some(request.max_output_tokens),
        })
    }
}

impl ChatMessage {
    fn from_model_message(message: &ModelMessage) -> AppResult<Self> {
        match message.role {
            ModelRole::User => Ok(Self {
                role: ChatRole::User,
                content: Some(text_from_blocks(&message.content)),
                tool_calls: None,
                tool_call_id: None,
            }),
            ModelRole::Assistant => {
                let text = text_from_blocks(&message.content);
                let tool_calls = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ModelBlock::ToolUse { id, name, input } => Some(ChatToolCall {
                            id: id.clone(),
                            tool_type: ChatToolType::Function,
                            function: ChatFunctionCall {
                                name: provider_tool_name(name).into(),
                                arguments: serde_json::to_string(input).unwrap_or_default(),
                            },
                        }),
                        ModelBlock::Text { .. } | ModelBlock::ToolResult { .. } => None,
                    })
                    .collect::<Vec<_>>();
                Ok(Self {
                    role: ChatRole::Assistant,
                    content: (!text.is_empty()).then_some(text),
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                    tool_call_id: None,
                })
            }
            ModelRole::Tool => {
                let result = message.content.iter().find_map(|block| match block {
                    ModelBlock::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    } => Some((tool_call_id, content)),
                    ModelBlock::Text { .. } | ModelBlock::ToolUse { .. } => None,
                });
                let (tool_call_id, content) = result.ok_or_else(|| {
                    AppError::Provider("tool message did not contain a tool result".into())
                })?;
                Ok(Self {
                    role: ChatRole::Tool,
                    content: Some(content.clone()),
                    tool_calls: None,
                    tool_call_id: Some(tool_call_id.clone()),
                })
            }
        }
    }
}

impl ChatTool {
    fn from_tool_spec(spec: &ToolSpec) -> Self {
        Self {
            tool_type: ChatToolType::Function,
            function: ChatFunctionDefinition {
                name: provider_tool_name(&spec.name).into(),
                description: spec.description.clone(),
                parameters: spec.input_schema.clone(),
            },
        }
    }
}

impl ChatCompletionResponse {
    fn into_model_response(self) -> AppResult<ModelResponse> {
        let choice = self
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Provider("provider returned no choices".into()))?;
        let mut content = Vec::new();
        if let Some(text) = choice.message.content.filter(|text| !text.is_empty()) {
            content.push(ModelBlock::Text { text });
        }
        for call in choice.message.tool_calls.unwrap_or_default() {
            let tool_name = internal_tool_name(&call.function.name).ok_or_else(|| {
                AppError::Provider(format!(
                    "provider returned unknown tool {}",
                    call.function.name
                ))
            })?;
            let input = serde_json::from_str(&call.function.arguments).map_err(|error| {
                AppError::Provider(format!(
                    "provider returned invalid JSON for {}: {error}",
                    call.function.name
                ))
            })?;
            content.push(ModelBlock::ToolUse {
                id: call.id,
                name: tool_name.into(),
                input,
            });
        }
        let stop = match choice.finish_reason {
            ChatFinishReason::Stop => ModelStop::EndTurn,
            ChatFinishReason::ToolCalls | ChatFinishReason::FunctionCall => ModelStop::ToolUse,
            ChatFinishReason::Length => ModelStop::MaxOutput,
            ChatFinishReason::ContentFilter => ModelStop::ContentFilter,
        };
        let usage = self.usage.unwrap_or(ChatUsage {
            prompt_tokens: Some(0),
            completion_tokens: Some(0),
        });

        Ok(ModelResponse {
            content,
            stop,
            usage: ModelUsage {
                input_tokens: usage.prompt_tokens.unwrap_or(0),
                output_tokens: usage.completion_tokens.unwrap_or(0),
            },
        })
    }
}

pub fn tool_specs(enabled_tools: &[String]) -> Vec<ToolSpec> {
    enabled_tools
        .iter()
        .filter_map(|tool| match tool.as_str() {
            "file.read" => Some(ToolSpec {
                name: "file_read".into(),
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
            "file.write" => Some(ToolSpec {
                name: "file_write".into(),
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
    "You are Plato Agent. Use at most one tool call in a response. Use file_read when you need to inspect workspace files. Use file_write only when the user explicitly asks you to write or edit a file. After a tool result, answer the user directly or request exactly one next tool call."
}

fn provider_tool_name(internal_name: &str) -> &str {
    match internal_name {
        "file.read" => "file_read",
        "file.write" => "file_write",
        other => other,
    }
}

fn internal_tool_name(provider_name: &str) -> Option<&'static str> {
    match provider_name {
        "file_read" | "file.read" => Some("file.read"),
        "file_write" | "file.write" => Some("file.write"),
        _ => None,
    }
}

fn text_from_blocks(blocks: &[ModelBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ModelBlock::Text { text } => Some(text.as_str()),
            ModelBlock::ToolUse { .. } | ModelBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_and_tool_uses_from_response() {
        let response: ModelResponse = serde_json::from_value(json!({
            "stop": "tool_use",
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

    #[test]
    fn maps_openai_tool_calls_to_internal_tool_names() {
        let response: ChatCompletionResponse = serde_json::from_value(json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "I will read it.",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "file_read",
                            "arguments": "{\"path\":\"README.md\"}"
                        }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        }))
        .unwrap();

        let response = response.into_model_response().unwrap();

        assert_eq!(response.stop, ModelStop::ToolUse);
        assert_eq!(
            response.tool_uses(),
            vec![(
                "call_1".into(),
                "file.read".into(),
                json!({"path": "README.md"})
            )]
        );
    }

    #[test]
    fn maps_model_messages_to_chat_completion_messages() {
        let message = ModelMessage::assistant_blocks(vec![
            ModelBlock::Text {
                text: "Reading".into(),
            },
            ModelBlock::ToolUse {
                id: "call_1".into(),
                name: "file.read".into(),
                input: json!({"path": "README.md"}),
            },
        ]);

        let chat = ChatMessage::from_model_message(&message).unwrap();

        assert!(matches!(chat.role, ChatRole::Assistant));
        assert_eq!(chat.content, Some("Reading".into()));
        assert_eq!(chat.tool_calls.unwrap()[0].function.name, "file_read");
    }
}
