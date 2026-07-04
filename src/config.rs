use crate::{AppError, AppResult};
use serde::Deserialize;
use std::{fs, path::Path};

const DEFAULT_MODEL: &str = "claude-sonnet-5";
const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const DEFAULT_TOKEN_BUDGET: u32 = 4_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub provider: ProviderConfig,
    pub limits: LimitsConfig,
    pub tools: ToolsConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderConfig {
    pub model: String,
    pub api_key_env: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitsConfig {
    pub token_budget: u32,
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
    model: Option<String>,
    api_key_env: Option<String>,
}

#[derive(Default, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimitsConfig {
    token_budget: Option<u32>,
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
                model: provider.model.unwrap_or_else(|| DEFAULT_MODEL.into()),
                api_key_env: provider
                    .api_key_env
                    .unwrap_or_else(|| DEFAULT_API_KEY_ENV.into()),
            },
            limits: LimitsConfig { token_budget },
            tools: ToolsConfig { enabled },
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: ProviderConfig {
                model: DEFAULT_MODEL.into(),
                api_key_env: DEFAULT_API_KEY_ENV.into(),
            },
            limits: LimitsConfig {
                token_budget: DEFAULT_TOKEN_BUDGET,
            },
            tools: ToolsConfig {
                enabled: vec!["file.read".into(), "file.write".into()],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_the_two_bootstrap_tools() {
        let config = Config::default();

        assert_eq!(config.provider.api_key_env, "ANTHROPIC_API_KEY");
        assert_eq!(config.tools.enabled, vec!["file.read", "file.write"]);
    }

    #[test]
    fn rejects_zero_token_budget() {
        let raw = RawConfig {
            provider: None,
            limits: Some(RawLimitsConfig {
                token_budget: Some(0),
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
}
