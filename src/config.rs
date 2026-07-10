use crate::{
    AppError, AppResult,
    tool_catalog::{default_enabled_tools, is_known_tool},
};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

const DEFAULT_OPENAI_MODEL: &str = "gpt-5.5";
const DEFAULT_OPENROUTER_MODEL: &str = "~openai/gpt-latest";
const DEFAULT_TOKEN_BUDGET: u32 = 4_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 1_024;
const DEFAULT_MAX_TURNS: u32 = 8;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
const PLATO_CONFIG_ENV: &str = "PLATO_CONFIG";

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
    pub max_turns: u32,
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
    max_turns: Option<u32>,
}

#[derive(Default, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolsConfig {
    enabled: Option<Vec<String>>,
}

impl Config {
    pub fn load(workspace_root: &Path, explicit_path: Option<&Path>) -> AppResult<Self> {
        let Some(path) = resolve_config_path(workspace_root, explicit_path)? else {
            return Ok(Self::default());
        };
        Self::load_file(&path)
    }

    fn load_file(path: &Path) -> AppResult<Self> {
        let raw = fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&raw)?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> AppResult<Self> {
        let provider = raw.provider.unwrap_or_default();
        let limits = raw.limits.unwrap_or_default();
        let tools = raw.tools.unwrap_or_default();
        let token_budget = positive(
            limits.token_budget.unwrap_or(DEFAULT_TOKEN_BUDGET),
            "limits.token_budget",
        )?;
        let max_output_tokens = positive(
            limits
                .max_output_tokens
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
            "limits.max_output_tokens",
        )?;
        let max_turns = positive(
            limits.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
            "limits.max_turns",
        )?;
        let timeout_ms = positive(
            provider.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            "provider.timeout_ms",
        )?;
        let kind = provider.kind.unwrap_or(ProviderKind::OpenRouter);

        let enabled = tools.enabled.unwrap_or_else(default_enabled_tools);
        if enabled.is_empty() {
            return Err(AppError::Config("tools.enabled must not be empty".into()));
        }
        if let Some(tool) = enabled.iter().find(|tool| !is_known_tool(tool)) {
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
                max_turns,
            },
            tools: ToolsConfig { enabled },
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        let kind = ProviderKind::OpenRouter;
        Self {
            provider: ProviderConfig {
                model: default_model(&kind).into(),
                api_key_env: default_api_key_env(&kind).into(),
                base_url: default_base_url(&kind).into(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
                http_referer: None,
                app_title: None,
                kind,
            },
            limits: LimitsConfig {
                token_budget: DEFAULT_TOKEN_BUDGET,
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                max_turns: DEFAULT_MAX_TURNS,
            },
            tools: ToolsConfig {
                enabled: default_enabled_tools(),
            },
        }
    }
}

fn positive<T: From<u8> + PartialEq>(value: T, field: &str) -> AppResult<T> {
    if value == T::from(0) {
        return Err(AppError::Config(format!("{field} must be positive")));
    }
    Ok(value)
}

fn default_model(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAi => DEFAULT_OPENAI_MODEL,
        ProviderKind::OpenRouter => DEFAULT_OPENROUTER_MODEL,
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

pub fn resolve_config_path(
    workspace_root: &Path,
    explicit_path: Option<&Path>,
) -> AppResult<Option<PathBuf>> {
    resolve_config_path_with(
        workspace_root,
        explicit_path.map(Path::to_path_buf),
        std::env::var_os(PLATO_CONFIG_ENV).map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn resolve_config_path_with(
    workspace_root: &Path,
    explicit_path: Option<PathBuf>,
    env_path: Option<PathBuf>,
    home: Option<PathBuf>,
) -> AppResult<Option<PathBuf>> {
    if let Some(path) = explicit_path {
        return resolve_explicit_config_path(workspace_root, path, home.as_deref()).map(Some);
    }
    if let Some(path) = env_path {
        return resolve_explicit_config_path(workspace_root, path, home.as_deref()).map(Some);
    }

    let workspace_config = workspace_root.join("plato.toml");
    if workspace_config.exists() {
        return Ok(Some(workspace_config));
    }

    if let Some(home) = home {
        let user_config = home.join(".config").join("plato").join("config.toml");
        if user_config.exists() {
            return Ok(Some(user_config));
        }
    }

    Ok(None)
}

fn resolve_explicit_config_path(
    workspace_root: &Path,
    path: PathBuf,
    home: Option<&Path>,
) -> AppResult<PathBuf> {
    let path = expand_leading_tilde(path, home)?;
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(workspace_root.join(path))
    }
}

fn expand_leading_tilde(path: PathBuf, home: Option<&Path>) -> AppResult<PathBuf> {
    let Some(raw) = path.to_str() else {
        return Ok(path);
    };
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .ok_or_else(|| AppError::Config("HOME is required for ~ expansion".into()));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        let home =
            home.ok_or_else(|| AppError::Config("HOME is required for ~ expansion".into()))?;
        return Ok(home.join(rest));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_the_bootstrap_tools() {
        let config = Config::default();

        assert_eq!(config.provider.kind, ProviderKind::OpenRouter);
        assert_eq!(config.provider.model, "~openai/gpt-latest");
        assert_eq!(config.provider.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(config.provider.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(config.limits.max_turns, 8);
        assert_eq!(
            config.tools.enabled,
            vec![
                "file.read",
                "file.list",
                "file.write",
                "file.edit",
                "shell.exec"
            ]
        );
    }

    #[test]
    fn rejects_zero_token_budget() {
        let raw = RawConfig {
            provider: None,
            limits: Some(RawLimitsConfig {
                token_budget: Some(0),
                max_output_tokens: None,
                max_turns: None,
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
                max_turns: None,
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
                enabled: Some(vec!["shell.delete".into()]),
            }),
        };

        let err = Config::from_raw(raw).unwrap_err();

        assert!(matches!(
            err,
            AppError::Config(message) if message == "unknown tool in tools.enabled: shell.delete"
        ));
    }

    #[test]
    fn rejects_zero_max_turns() {
        let raw = RawConfig {
            provider: None,
            limits: Some(RawLimitsConfig {
                token_budget: None,
                max_output_tokens: None,
                max_turns: Some(0),
            }),
            tools: None,
        };

        assert!(matches!(Config::from_raw(raw), Err(AppError::Config(_))));
    }

    #[test]
    fn parses_configured_max_turns() {
        let raw = RawConfig {
            provider: None,
            limits: Some(RawLimitsConfig {
                token_budget: None,
                max_output_tokens: None,
                max_turns: Some(3),
            }),
            tools: None,
        };

        assert_eq!(Config::from_raw(raw).unwrap().limits.max_turns, 3);
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

    #[test]
    fn explicit_config_path_wins_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let explicit = dir.path().join("explicit.toml");
        std::fs::write(dir.path().join("plato.toml"), "").unwrap();

        let path = resolve_config_path_with(
            dir.path(),
            Some(explicit.clone()),
            Some(PathBuf::from("env.toml")),
            Some(home.path().to_path_buf()),
        )
        .unwrap();

        assert_eq!(path, Some(explicit));
    }

    #[test]
    fn plato_config_env_is_second_resolution_step() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("env.toml");
        std::fs::write(dir.path().join("plato.toml"), "").unwrap();

        let path = resolve_config_path_with(
            dir.path(),
            None,
            Some(env_path.clone()),
            Some(home.path().to_path_buf()),
        )
        .unwrap();

        assert_eq!(path, Some(env_path));
    }

    #[test]
    fn workspace_plato_toml_is_third_resolution_step() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let workspace_config = dir.path().join("plato.toml");
        std::fs::write(&workspace_config, "").unwrap();

        let path =
            resolve_config_path_with(dir.path(), None, None, Some(home.path().to_path_buf()))
                .unwrap();

        assert_eq!(path, Some(workspace_config));
    }

    #[test]
    fn user_config_is_fourth_resolution_step() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let user_config = home
            .path()
            .join(".config")
            .join("plato")
            .join("config.toml");
        std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
        std::fs::write(&user_config, "").unwrap();

        let path =
            resolve_config_path_with(dir.path(), None, None, Some(home.path().to_path_buf()))
                .unwrap();

        assert_eq!(path, Some(user_config));
    }

    #[test]
    fn missing_config_paths_resolve_to_built_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();

        let path =
            resolve_config_path_with(dir.path(), None, None, Some(home.path().to_path_buf()))
                .unwrap();

        assert_eq!(path, None);
    }

    #[test]
    fn expands_leading_tilde_for_explicit_config_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();

        let path = resolve_config_path_with(
            workspace.path(),
            Some(PathBuf::from("~/plato.toml")),
            None,
            Some(home.path().to_path_buf()),
        )
        .unwrap();

        assert_eq!(path, Some(home.path().join("plato.toml")));
    }

    #[test]
    fn relative_explicit_config_paths_resolve_against_workspace_root() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();

        let path = resolve_config_path_with(
            workspace.path(),
            Some(PathBuf::from("config/plato.toml")),
            None,
            Some(home.path().to_path_buf()),
        )
        .unwrap();

        assert_eq!(
            path,
            Some(workspace.path().join("config").join("plato.toml"))
        );
    }
}
