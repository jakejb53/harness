//! `config.toml` and `.env`, both read from the harness directory (`harness_dir`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

const CONFIG_FILE: &str = "config.toml";
const ENV_FILE: &str = ".env";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    endpoint: Endpoint,
    chat: Chat,
    repl: Repl,
    commands: Commands,
    approval_description: Option<ApprovalDescription>,
    theme: Theme,
}

/// Explicit foreground-only styles. No names, terminal defaults, or dim modifiers are accepted.
#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    #[serde(deserialize_with = "theme_style")]
    pub system: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub user: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub assistant: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub reasoning: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub tool_call: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub tool_result: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub approval: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub warning: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub error: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub status: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub context_turn: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub context_tokens: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub context_role: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub context_message_tokens: ratatui::style::Style,
    #[serde(deserialize_with = "theme_style")]
    pub input: ratatui::style::Style,
}

/// Reject malformed colors before slicing, and resolve RGB without terminal color-name mappings.
fn theme_style<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ratatui::style::Style, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.len() != 7
        || !value.starts_with('#')
        || !value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
    {
        return Err(serde::de::Error::custom(
            "expected a color in #RRGGBB format",
        ));
    }
    let rgb = u32::from_str_radix(&value[1..], 16).map_err(serde::de::Error::custom)?;
    Ok(ratatui::style::Style::new().fg(ratatui::style::Color::Rgb(
        (rgb >> 16) as u8,
        (rgb >> 8) as u8,
        rgb as u8,
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    /// OpenAI-compatible base URL, e.g. `https://api.openai.com/v1`.
    base_url: String,
    model: String,
    /// Name of the environment variable holding the API key. Omitted means the
    /// request is sent without an `Authorization` header.
    api_key_env: Option<String>,
    /// Optional request fields; omission sends no parameter overrides.
    parameters: Option<toml::Table>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalDescription {
    enabled: bool,
    prompt: Option<String>,
    endpoint: Option<Endpoint>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Chat {
    /// System message sent ahead of the conversation in every chat request, never
    /// saved to the session. Omitted means no system message.
    prompt: Option<String>,
    /// Send earlier turns' reasoning as `reasoning()` REPL calls (`tool_reasoning`).
    /// Rewritten by `/tool-reasoning`.
    tool_reasoning: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commands {
    /// Denied names take precedence over allowed names when commands are brokered.
    pub deny: Vec<String>,
    /// Exact environment variable names withheld from command processes.
    pub env_filter: Vec<String>,
    /// Model-facing command names mapped to absolute executable paths.
    pub allow: BTreeMap<String, PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repl {
    /// Model-facing tool description, required and sent verbatim on every request.
    tool_description: String,
    permission: Permission,
    /// Worker memory limit in MiB; conversion to bytes belongs to the runtime.
    max_memory_mb: usize,
    /// Maximum bytes of REPL output sent to the model per call.
    output_limit: usize,
}

/// REPL filesystem access inside the repository; outside access is denied at every level.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// Deny repository reads and writes.
    None,
    /// Allow repository reads, deny writes.
    ReadOnly,
    /// Allow repository reads and writes.
    ReadWrite,
}

impl Permission {
    /// The configuration spelling, also shown in the status line.
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::None => "none",
            Permission::ReadOnly => "read-only",
            Permission::ReadWrite => "read-write",
        }
    }

    /// Shift+Tab order: none → read-only → read-write → none.
    pub fn next(self) -> Self {
        match self {
            Permission::None => Permission::ReadOnly,
            Permission::ReadOnly => Permission::ReadWrite,
            Permission::ReadWrite => Permission::None,
        }
    }
}

/// Endpoint settings resolved for sending requests.
#[derive(Clone)]
pub struct ResolvedEndpoint {
    pub url: String,
    pub tokenize_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// JSON-compatible request fields, validated against protocol-owned keys.
    pub parameters: serde_json::Map<String, serde_json::Value>,
}

/// Independent endpoint and template for display-only explanations of pending operations.
#[derive(Clone)]
pub struct ApprovalDescriptionSettings {
    pub endpoint: ResolvedEndpoint,
    pub prompt: String,
}

pub struct Settings {
    /// The harness directory the settings were loaded from (`harness_dir`).
    pub dir: PathBuf,
    pub endpoint: ResolvedEndpoint,
    pub chat_prompt: Option<String>,
    pub tool_reasoning: bool,
    pub repl_tool_description: String,
    pub permission: Permission,
    pub max_memory_mb: usize,
    pub commands: Commands,
    pub output_limit: usize,
    pub approval_description: Option<ApprovalDescriptionSettings>,
    pub theme: Theme,
}

/// `~/.harness`: config, `.env`, sessions, and `replib/`, shared by every directory the
/// app runs in.
pub fn harness_dir() -> anyhow::Result<PathBuf> {
    std::env::home_dir()
        .map(|home| home.join(".harness"))
        .context("can't find the home directory")
}

/// Require explicit operational settings and resolve credentials without exporting secrets.
pub fn load(dir: &Path) -> anyhow::Result<Settings> {
    let config_path = dir.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", config_path.display()))?;

    // Command execution must use the configured executable, never resolve an allow
    // entry relative to the repository or through PATH.
    for (name, path) in &config.commands.allow {
        if !path.is_absolute() {
            bail!(
                "{}: commands.allow.{name} must be an absolute path, got {}",
                config_path.display(),
                path.display()
            );
        }
    }

    // `.env` is read privately, never exported to the app's environment or its
    // child processes. A missing `.env` is fine.
    let env_path = dir.join(ENV_FILE);
    let mut dotenv = HashMap::new();
    if env_path.exists() {
        for item in dotenvy::from_path_iter(&env_path)
            .with_context(|| format!("loading {}", env_path.display()))?
        {
            let (key, value) = item.with_context(|| format!("parsing {}", env_path.display()))?;
            dotenv.insert(key, value);
        }
    }

    let endpoint = resolve_endpoint(
        config.endpoint,
        "endpoint",
        &dotenv,
        &config_path,
        &env_path,
    )?;
    let approval_description = resolve_approval_description(
        config.approval_description,
        &dotenv,
        &config_path,
        &env_path,
    )?;

    Ok(Settings {
        dir: dir.to_path_buf(),
        endpoint,
        approval_description,
        theme: config.theme,
        chat_prompt: config.chat.prompt,
        tool_reasoning: config.chat.tool_reasoning,
        repl_tool_description: config.repl.tool_description,
        permission: config.repl.permission,
        max_memory_mb: config.repl.max_memory_mb,
        commands: config.commands,
        output_limit: config.repl.output_limit,
    })
}

/// Resolve either endpoint without consuming credentials shared by both configurations.
fn resolve_endpoint(
    endpoint: Endpoint,
    field: &str,
    dotenv: &HashMap<String, String>,
    config_path: &Path,
    env_path: &Path,
) -> anyhow::Result<ResolvedEndpoint> {
    let parameters = endpoint_parameters(endpoint.parameters.as_ref(), field)
        .with_context(|| format!("parsing {}", config_path.display()))?;
    let api_key = match &endpoint.api_key_env {
        // Variables already set in the shell take precedence over `.env`.
        Some(name) => match std::env::var(name)
            .ok()
            .or_else(|| dotenv.get(name).cloned())
        {
            Some(key) => Some(key),
            None => bail!(
                "{}: {field}.api_key_env names {name}, which is not set in the environment or {}",
                config_path.display(),
                env_path.display()
            ),
        },
        None => None,
    };
    let base_url = endpoint.base_url.trim_end_matches('/');
    let tokenizer_root = base_url.strip_suffix("/v1").unwrap_or(base_url);
    Ok(ResolvedEndpoint {
        url: format!("{base_url}/chat/completions"),
        tokenize_url: format!("{tokenizer_root}/tokenize"),
        model: endpoint.model,
        api_key,
        parameters,
    })
}

/// An enabled feature requires its own endpoint and an operation-bearing prompt.
fn resolve_approval_description(
    config: Option<ApprovalDescription>,
    dotenv: &HashMap<String, String>,
    config_path: &Path,
    env_path: &Path,
) -> anyhow::Result<Option<ApprovalDescriptionSettings>> {
    let Some(config) = config.filter(|config| config.enabled) else {
        return Ok(None);
    };
    let prompt = config.prompt.with_context(|| {
        format!(
            "{}: approval_description.prompt is required when enabled",
            config_path.display()
        )
    })?;
    if !prompt.contains("${command}") {
        bail!(
            "{}: approval_description.prompt must contain ${{command}}",
            config_path.display()
        );
    }
    let endpoint = config.endpoint.with_context(|| {
        format!(
            "{}: approval_description.endpoint is required when enabled",
            config_path.display()
        )
    })?;
    let endpoint = resolve_endpoint(
        endpoint,
        "approval_description.endpoint",
        dotenv,
        config_path,
        env_path,
    )?;
    Ok(Some(ApprovalDescriptionSettings { endpoint, prompt }))
}

/// Reserve protocol-owned fields while leaving endpoint-specific names to the server.
fn endpoint_parameters(
    table: Option<&toml::Table>,
    field: &str,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let mut parameters = serde_json::Map::new();
    if let Some(table) = table {
        for (name, value) in table {
            if matches!(
                name.as_str(),
                "model" | "messages" | "tools" | "stream" | "stream_options" | "n"
            ) {
                bail!("{field}.parameters.{name} is reserved by Harness");
            }
            parameters.insert(
                name.clone(),
                json_parameter(value, &format!("{field}.parameters.{name}"))?,
            );
        }
    }
    Ok(parameters)
}

/// Convert without silently turning non-finite numbers into null or dates into objects.
fn json_parameter(value: &toml::Value, key: &str) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value;
    Ok(match value {
        toml::Value::String(value) => Value::String(value.clone()),
        toml::Value::Integer(value) => Value::from(*value),
        toml::Value::Float(value) => Value::Number(
            serde_json::Number::from_f64(*value)
                .with_context(|| format!("{key}: non-finite numbers cannot be sent as JSON"))?,
        ),
        toml::Value::Boolean(value) => Value::Bool(*value),
        toml::Value::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| json_parameter(value, &format!("{key}[{index}]")))
                .collect::<anyhow::Result<_>>()?,
        ),
        toml::Value::Table(values) => Value::Object(
            values
                .iter()
                .map(|(name, value)| {
                    Ok((
                        name.clone(),
                        json_parameter(value, &format!("{key}.{name}"))?,
                    ))
                })
                .collect::<anyhow::Result<_>>()?,
        ),
        toml::Value::Datetime(_) => {
            bail!("{key}: TOML dates and times cannot be sent as JSON; use a quoted string")
        }
    })
}

/// Persist `permission` (Shift+Tab).
pub fn save_permission(dir: &Path, permission: Permission) -> anyhow::Result<()> {
    save_setting(dir, "repl", "permission", permission.as_str().into())
}

/// Persist `tool_reasoning` (`/tool-reasoning`).
pub fn save_tool_reasoning(dir: &Path, enabled: bool) -> anyhow::Result<()> {
    save_setting(dir, "chat", "tool_reasoning", enabled.into())
}

/// Change only `[table] key` (creating the table if missing) so comments and
/// formatting elsewhere in the file are preserved.
fn save_setting(dir: &Path, table: &str, key: &str, new: toml_edit::Value) -> anyhow::Result<()> {
    let config_path = dir.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", config_path.display()))?;
    // Table-like covers both `[repl]` and `repl = { ... }`.
    let settings = doc
        .entry(table)
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .with_context(|| format!("{}: `{table}` is not a table", config_path.display()))?;
    match settings
        .get_mut(key)
        .and_then(toml_edit::Item::as_value_mut)
    {
        // Replace the value in place, keeping its decor (e.g. a trailing comment).
        Some(value) => {
            let decor = value.decor().clone();
            *value = new;
            *value.decor_mut() = decor;
        }
        None => {
            settings.insert(key, toml_edit::Item::Value(new));
        }
    }

    // Temp file and rename, so a crash never leaves a truncated config.
    use std::io::Write;
    let tmp = config_path.with_extension("toml.tmp");
    let mut file =
        std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(doc.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &config_path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), config_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Omission adds no defaults; configured scalar and structured values retain their types.
    #[test]
    fn endpoint_parameters_preserve_values() {
        assert!(endpoint_parameters(None, "endpoint").unwrap().is_empty());
        let table = toml::from_str("temperature = 0.7\ntop_p = 0.9\ntop_k = 40\npresence_penalty = 0.5\nrepetition_penalty = 1.1\nmax_tokens = 8192\nstop = ['END']\ncustom = { enabled = true }\n").unwrap();
        assert_eq!(
            serde_json::Value::Object(endpoint_parameters(Some(&table), "endpoint").unwrap()),
            serde_json::json!({
                "temperature": 0.7, "top_p": 0.9, "top_k": 40, "presence_penalty": 0.5,
                "repetition_penalty": 1.1, "max_tokens": 8192, "stop": ["END"], "custom": {"enabled": true}
            })
        );
    }

    /// Protocol fields must fail explicitly rather than override the client's required fields.
    #[test]
    fn endpoint_parameters_reject_reserved_fields() {
        for name in [
            "model",
            "messages",
            "tools",
            "stream",
            "stream_options",
            "n",
        ] {
            let table = toml::from_str(&format!("{name} = 1")).unwrap();
            assert_eq!(
                endpoint_parameters(Some(&table), "endpoint")
                    .unwrap_err()
                    .to_string(),
                format!("endpoint.parameters.{name} is reserved by Harness")
            );
        }
    }

    /// Nested invalid values must fail with their full location, never become JSON null.
    #[test]
    fn endpoint_parameters_reject_non_json_values() {
        for (source, expected) in [
            (
                "temperature = nan",
                "endpoint.parameters.temperature: non-finite numbers cannot be sent as JSON",
            ),
            (
                "custom = { values = [inf] }",
                "endpoint.parameters.custom.values[0]: non-finite numbers cannot be sent as JSON",
            ),
            (
                "custom = { date = 2026-09-28 }",
                "endpoint.parameters.custom.date: TOML dates and times cannot be sent as JSON; use a quoted string",
            ),
        ] {
            let table = toml::from_str(source).unwrap();
            assert_eq!(
                endpoint_parameters(Some(&table), "endpoint")
                    .unwrap_err()
                    .to_string(),
                expected
            );
        }
    }

    /// Enabled descriptions need explicit configuration; disabled and absent sections need none.
    #[test]
    fn approval_description_configuration() {
        let dotenv = HashMap::new();
        let resolve = |source: Option<&str>| {
            resolve_approval_description(
                source.map(|source| toml::from_str(source).unwrap()),
                &dotenv,
                Path::new("config.toml"),
                Path::new(".env"),
            )
        };
        assert!(resolve(None).unwrap().is_none());
        assert!(resolve(Some("enabled = false")).unwrap().is_none());
        for (source, key) in [
            ("enabled = true", "approval_description.prompt"),
            ("enabled = true\nprompt = 'Explain this'", "${command}"),
            (
                "enabled = true\nprompt = '${command}'",
                "approval_description.endpoint",
            ),
        ] {
            let error = resolve(Some(source))
                .err()
                .expect("invalid config accepted")
                .to_string();
            assert!(error.contains(key), "{error}");
        }
        let resolved = resolve(Some("enabled = true\nprompt = 'Describe ${command}'\n[endpoint]\nbase_url = 'http://description.test/v1/'\nmodel = 'describer'\n[endpoint.parameters]\ntemperature = 0.1")).unwrap().unwrap();
        assert_eq!(
            resolved.endpoint.url,
            "http://description.test/v1/chat/completions"
        );
        assert_eq!(
            resolved.endpoint.tokenize_url,
            "http://description.test/tokenize"
        );
        assert_eq!(resolved.endpoint.model, "describer");
        assert_eq!(resolved.endpoint.parameters["temperature"], 0.1);
        let example: Config = toml::from_str(include_str!("../config.toml.example")).unwrap();
        assert!(!example.approval_description.unwrap().enabled);
    }

    /// Sharing an API-key name must not consume its private dotenv entry after the first endpoint.
    #[test]
    fn endpoints_can_share_dotenv_credentials() {
        let name = "HARNESS_DESCRIPTION_TEST_CREDENTIAL_7813";
        assert!(std::env::var_os(name).is_none());
        let dotenv = HashMap::from([(name.to_owned(), "test-value".to_owned())]);
        for field in ["endpoint", "approval_description.endpoint"] {
            let endpoint: Endpoint = toml::from_str(&format!(
                "base_url = 'http://example.test'\nmodel = 'test'\napi_key_env = '{name}'"
            ))
            .unwrap();
            let resolved = resolve_endpoint(
                endpoint,
                field,
                &dotenv,
                Path::new("config.toml"),
                Path::new(".env"),
            )
            .unwrap();
            assert_eq!(resolved.api_key.as_deref(), Some("test-value"));
        }
        assert_eq!(dotenv.len(), 1);
    }

    /// RGB input is exact and foreground-only; all roles must be configured explicitly.
    #[test]
    fn theme_requires_complete_hex_palette() {
        let config: toml::Value = toml::from_str(include_str!("../config.toml.example")).unwrap();
        let valid = config["theme"].clone();
        for invalid in [
            "magenta",
            "default",
            "#abc",
            "#12345678",
            "123456",
            "#GG1234",
            " #123456",
            "#éabcd",
        ] {
            let mut theme = valid.clone();
            theme["reasoning"] = invalid.into();
            let result: Result<Theme, _> = theme.try_into();
            assert!(result.is_err(), "accepted invalid color: {invalid}");
        }
        for role in valid.as_table().unwrap().keys() {
            let mut theme = valid.clone();
            theme.as_table_mut().unwrap().remove(role);
            let result: Result<Theme, _> = theme.try_into();
            assert!(result.is_err(), "accepted missing role: {role}");
        }
        let mut lower = valid;
        lower["reasoning"] = "#d7afff".into();
        let theme: Theme = lower.try_into().unwrap();
        assert_eq!(
            theme.reasoning.fg,
            Some(ratatui::style::Color::Rgb(215, 175, 255))
        );
        assert!(theme.reasoning.add_modifier.is_empty());
        assert!(theme.reasoning.bg.is_none());
        let mut missing = config;
        missing.as_table_mut().unwrap().remove("theme");
        let result: Result<Config, _> = missing.try_into();
        assert!(result.is_err());
    }
}
