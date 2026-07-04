use platonic_core::EffectClass;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const FILE_READ: &str = "file.read";
pub const FILE_WRITE: &str = "file.write";

const PROVIDER_FILE_READ: &str = "file_read";
const PROVIDER_FILE_WRITE: &str = "file_write";

const BOOTSTRAP_TOOLS: &[ToolDefinition] = &[
    ToolDefinition {
        internal_name: FILE_READ,
        provider_name: PROVIDER_FILE_READ,
        effect: EffectClass::ReadOnly,
        description: "Read a UTF-8 text file inside the current workspace.",
        input_schema: ToolInputSchema::FileRead,
    },
    ToolDefinition {
        internal_name: FILE_WRITE,
        provider_name: PROVIDER_FILE_WRITE,
        effect: EffectClass::WorkspaceWrite,
        description: "Write UTF-8 text to a relative path inside the current workspace after approval.",
        input_schema: ToolInputSchema::FileWrite,
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDefinition {
    pub internal_name: &'static str,
    pub provider_name: &'static str,
    pub effect: EffectClass,
    pub description: &'static str,
    input_schema: ToolInputSchema,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolInputSchema {
    FileRead,
    FileWrite,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub fn bootstrap_tools() -> &'static [ToolDefinition] {
    BOOTSTRAP_TOOLS
}

pub fn default_enabled_tools() -> Vec<String> {
    BOOTSTRAP_TOOLS
        .iter()
        .map(|tool| tool.internal_name.into())
        .collect()
}

pub fn is_known_tool(name: &str) -> bool {
    lookup_internal(name).is_some()
}

pub fn effect_for_tool(name: &str) -> EffectClass {
    lookup_internal(name)
        .map(|tool| tool.effect.clone())
        .unwrap_or(EffectClass::ExternalSideEffect)
}

pub fn provider_name_for_internal(name: &str) -> Option<&'static str> {
    lookup_internal(name).map(|tool| tool.provider_name)
}

pub fn internal_name_for_provider(name: &str) -> Option<&'static str> {
    BOOTSTRAP_TOOLS
        .iter()
        .find(|tool| tool.provider_name == name || tool.internal_name == name)
        .map(|tool| tool.internal_name)
}

pub fn tool_specs(enabled_tools: &[String]) -> Vec<ToolSpec> {
    enabled_tools
        .iter()
        .filter_map(|name| lookup_internal(name))
        .map(ToolSpec::from_definition)
        .collect()
}

fn lookup_internal(name: &str) -> Option<&'static ToolDefinition> {
    BOOTSTRAP_TOOLS
        .iter()
        .find(|tool| tool.internal_name == name)
}

impl ToolSpec {
    fn from_definition(definition: &ToolDefinition) -> Self {
        Self {
            name: definition.provider_name.into(),
            description: definition.description.into(),
            input_schema: definition.input_schema.to_json(),
        }
    }
}

impl ToolInputSchema {
    fn to_json(self) -> Value {
        match self {
            Self::FileRead => json!({
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
            Self::FileWrite => json!({
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_catalog_is_exactly_file_read_and_file_write() {
        let actual = bootstrap_tools()
            .iter()
            .map(|tool| (tool.internal_name, tool.effect.clone()))
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![
                (FILE_READ, EffectClass::ReadOnly),
                (FILE_WRITE, EffectClass::WorkspaceWrite),
            ]
        );
        assert!(
            !actual
                .iter()
                .any(|(_, effect)| matches!(effect, EffectClass::Network))
        );
    }

    #[test]
    fn unknown_tool_effect_fails_closed() {
        assert_eq!(
            effect_for_tool("shell.exec"),
            EffectClass::ExternalSideEffect
        );
    }

    #[test]
    fn emits_provider_tool_specs_from_catalog() {
        let specs = tool_specs(&[FILE_READ.into(), FILE_WRITE.into()]);

        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, PROVIDER_FILE_READ);
        assert_eq!(specs[1].name, PROVIDER_FILE_WRITE);
    }
}
