use crate::{AppError, AppResult};
use serde::Deserialize;
use std::{fs, path::Path};

const DEFAULT_MODEL: &str = "gpt-5.5";
const DEFAULT_TOKEN_BUDGET: u32 = 4_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 1_024;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub provider: ProviderConfig,
    pub limits: LimitsConfig,
    pub tools: ToolsConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub model: String,
    pub api_key_env: String,
    pub base_url: String,
    pub timeout_ms: u64,
    pub http_referer: Option<String>,
    pub app_title: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenAi,
    OpenRouter,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitsConfig {
    pub token_budget: u32,
    pub max_output_tokens: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolsConfig {
    pub enabled: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    provider: Option<RawProviderConfig>,
    limits: Option<RawLimitsConfig>,
    tools: Option<RawToolsConfig>,
}

#[derive(Default, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProviderConfig {
    kind: Option<ProviderKind>,
    model: Option<String>,
    api_key_env: Option<String>,
    base_url: Option<String>,
    timeout_ms: Option<u64>,
    http_referer: Option<String>,
    app_title: Option<String>,
}

#[derive(Default, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimitsConfig {
    token_budget: Option<u32>,
    max_output_tokens: Option<u32>,
}

#[derive(Default, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolsConfig {
    enabled: Option<Vec<String>>,
}

impl Config {
    pub fn load(path: &Path) -> AppResult<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&raw)?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> AppResult<Self> {
        let provider = raw.provider.unwrap_or_default();
        let limits = raw.limits.unwrap_or_default();
        let tools = raw.tools.unwrap_or_default();
        let token_budget = limits.token_budget.unwrap_or(DEFAULT_TOKEN_BUDGET);
        if token_budget == 0 {
            return Err(AppError::Config(
                "limits.token_budget must be positive".into(),
            ));
        }
        let max_output_tokens = limits
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        if max_output_tokens == 0 {
            return Err(AppError::Config(
                "limits.max_output_tokens must be positive".into(),
            ));
        }
        let timeout_ms = provider.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if timeout_ms == 0 {
            return Err(AppError::Config(
                "provider.timeout_ms must be positive".into(),
            ));
        }
        let kind = provider.kind.unwrap_or(ProviderKind::OpenAi);

        let enabled = tools
            .enabled
            .unwrap_or_else(|| vec!["file.read".into(), "file.write".into()]);
        if enabled.is_empty() {
            return Err(AppError::Config("tools.enabled must not be empty".into()));
        }
        if let Some(tool) = enabled
            .iter()
            .find(|tool| !matches!(tool.as_str(), "file.read" | "file.write"))
        {
            return Err(AppError::Config(format!(
                "unknown tool in tools.enabled: {tool}"
            )));
        }

        Ok(Self {
            provider: ProviderConfig {
                model: provider
                    .model
                    .unwrap_or_else(|| default_model(&kind).into()),
                api_key_env: provider
                    .api_key_env
                    .unwrap_or_else(|| default_api_key_env(&kind).into()),
                base_url: provider
                    .base_url
                    .unwrap_or_else(|| default_base_url(&kind).into()),
                timeout_ms,
                http_referer: provider.http_referer,
                app_title: provider.app_title,
                kind,
            },
            limits: LimitsConfig {
                token_budget,
                max_output_tokens,
            },
            tools: ToolsConfig { enabled },
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: DEFAULT_MODEL.into(),
                api_key_env: "OPENAI_API_KEY".into(),
                base_url: OPENAI_BASE_URL.into(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
                http_referer: None,
                app_title: None,
            },
            limits: LimitsConfig {
                token_budget: DEFAULT_TOKEN_BUDGET,
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            },
            tools: ToolsConfig {
                enabled: vec!["file.read".into(), "file.write".into()],
            },
        }
    }
}

fn default_model(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAi => DEFAULT_MODEL,
        ProviderKind::OpenRouter => "~openai/gpt-latest",
    }
}

fn default_api_key_env(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAi => "OPENAI_API_KEY",
        ProviderKind::OpenRouter => "OPENROUTER_API_KEY",
    }
}

fn default_base_url(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAi => OPENAI_BASE_URL,
        ProviderKind::OpenRouter => OPENROUTER_BASE_URL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_the_two_bootstrap_tools() {
        let config = Config::default();

        assert_eq!(config.provider.api_key_env, "OPENAI_API_KEY");
        assert_eq!(config.provider.base_url, "https://api.openai.com/v1");
        assert_eq!(config.tools.enabled, vec!["file.read", "file.write"]);
    }

    #[test]
    fn rejects_zero_token_budget() {
        let raw = RawConfig {
            provider: None,
            limits: Some(RawLimitsConfig {
                token_budget: Some(0),
                max_output_tokens: None,
            }),
            tools: None,
        };

        assert!(matches!(Config::from_raw(raw), Err(AppError::Config(_))));
    }

    #[test]
    fn rejects_zero_max_output_tokens() {
        let raw = RawConfig {
            provider: None,
            limits: Some(RawLimitsConfig {
                token_budget: None,
                max_output_tokens: Some(0),
            }),
            tools: None,
        };

        assert!(matches!(Config::from_raw(raw), Err(AppError::Config(_))));
    }

    #[test]
    fn rejects_unknown_enabled_tools() {
        let raw = RawConfig {
            provider: None,
            limits: None,
            tools: Some(RawToolsConfig {
                enabled: Some(vec!["shell.exec".into()]),
            }),
        };

        assert!(matches!(Config::from_raw(raw), Err(AppError::Config(_))));
    }

    #[test]
    fn openrouter_defaults_to_openrouter_endpoint_and_key() {
        let raw = RawConfig {
            provider: Some(RawProviderConfig {
                kind: Some(ProviderKind::OpenRouter),
                model: None,
                api_key_env: None,
                base_url: None,
                timeout_ms: None,
                http_referer: None,
                app_title: None,
            }),
            limits: None,
            tools: None,
        };

        let config = Config::from_raw(raw).unwrap();

        assert_eq!(config.provider.model, "~openai/gpt-latest");
        assert_eq!(config.provider.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(config.provider.base_url, "https://openrouter.ai/api/v1");
    }
}
