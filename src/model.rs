use crate::tool_catalog::ToolSpec;
use platonic_core::ModelUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

pub fn system_prompt() -> &'static str {
    "You are Plato Agent. Use at most one tool call in a response. Use file_list when you need to inspect directory entries. Use file_read when you need to inspect workspace files. Use file_write only when the user explicitly asks you to write or edit a file. After a tool result, answer the user directly or request exactly one next tool call."
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
