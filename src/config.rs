use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Map as JsonMap, Number, Value as JsonValue};
use tokio::process::Command;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    servers: Vec<LanguageServerConfig>,
}

impl Config {
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        let raw =
            toml::from_str::<RawConfig>(input).map_err(ConfigError::Parse)?;
        raw.validate()
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let input =
            fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        Self::from_toml_str(&input)
    }

    pub fn servers(&self) -> &[LanguageServerConfig] {
        &self.servers
    }

    pub fn server(&self, name: &str) -> Option<&LanguageServerConfig> {
        self.servers.iter().find(|server| server.name == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LanguageServerConfig {
    name: String,
    command: String,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
    language_ids: Vec<String>,
    file_patterns: Vec<String>,
    initialization_options: JsonValue,
    timeouts: TimeoutConfig,
}

impl LanguageServerConfig {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub fn language_ids(&self) -> &[String] {
        &self.language_ids
    }

    pub fn file_patterns(&self) -> &[String] {
        &self.file_patterns
    }

    pub fn initialization_options(&self) -> &JsonValue {
        &self.initialization_options
    }

    pub fn timeouts(&self) -> TimeoutConfig {
        self.timeouts
    }

    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.command);
        command.args(&self.args).envs(&self.environment);
        command
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutConfig {
    request: Duration,
    shutdown: Duration,
}

impl TimeoutConfig {
    pub fn request(&self) -> Duration {
        self.request
    }

    pub fn shutdown(&self) -> Duration {
        self.shutdown
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to read config `{}`: {source}",
                    path.display()
                )
            }
            Self::Parse(source) => {
                write!(formatter, "failed to parse config TOML: {source}")
            }
            Self::Validation(message) => formatter.write_str(message),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Validation(_) => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    servers: Vec<RawLanguageServerConfig>,
}

impl RawConfig {
    fn validate(self) -> Result<Config, ConfigError> {
        if self.servers.is_empty() {
            return Err(validation("config must declare at least one server"));
        }

        let mut names = BTreeSet::new();
        let mut language_routes = BTreeMap::new();
        let mut pattern_routes = BTreeMap::new();
        let mut servers = Vec::with_capacity(self.servers.len());

        for server in self.servers {
            let name = require_non_empty("server name", server.name)?;
            if !names.insert(name.clone()) {
                return Err(validation(format!(
                    "duplicate server name `{name}`"
                )));
            }

            let command = require_non_empty(
                &format!("server `{name}` command"),
                server.command,
            )?;
            validate_environment(&name, &server.environment)?;
            validate_routes(
                &name,
                "language id",
                &server.language_ids,
                &mut language_routes,
            )?;
            validate_routes(
                &name,
                "file pattern",
                &server.file_patterns,
                &mut pattern_routes,
            )?;
            if server.language_ids.is_empty() && server.file_patterns.is_empty()
            {
                return Err(validation(format!(
                    "server `{name}` must declare language_ids, file_patterns, or both"
                )));
            }

            let initialization_options = match server.initialization_options {
                Some(value) => toml_to_json(
                    value,
                    &format!("servers.{name}.initialization_options"),
                )?,
                None => JsonValue::Object(JsonMap::new()),
            };

            servers.push(LanguageServerConfig {
                name,
                command,
                args: server.args,
                environment: server.environment,
                language_ids: server.language_ids,
                file_patterns: server.file_patterns,
                initialization_options,
                timeouts: server.timeouts.validate()?,
            });
        }

        Ok(Config { servers })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLanguageServerConfig {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    language_ids: Vec<String>,
    #[serde(default)]
    file_patterns: Vec<String>,
    #[serde(default)]
    initialization_options: Option<toml::Value>,
    #[serde(default)]
    timeouts: RawTimeoutConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTimeoutConfig {
    request_ms: Option<u64>,
    shutdown_ms: Option<u64>,
}

impl RawTimeoutConfig {
    fn validate(self) -> Result<TimeoutConfig, ConfigError> {
        Ok(TimeoutConfig {
            request: duration_or_default(
                "request_ms",
                self.request_ms,
                DEFAULT_REQUEST_TIMEOUT,
            )?,
            shutdown: duration_or_default(
                "shutdown_ms",
                self.shutdown_ms,
                DEFAULT_SHUTDOWN_TIMEOUT,
            )?,
        })
    }
}

fn require_non_empty(
    field: &str,
    value: String,
) -> Result<String, ConfigError> {
    if value.trim().is_empty() {
        return Err(validation(format!("{field} must not be empty")));
    }
    Ok(value)
}

fn validate_environment(
    server_name: &str,
    environment: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    for key in environment.keys() {
        if key.is_empty() {
            return Err(validation(format!(
                "server `{server_name}` environment keys must not be empty"
            )));
        }
        if key.contains('=') {
            return Err(validation(format!(
                "server `{server_name}` environment key `{key}` must not contain `=`"
            )));
        }
    }
    Ok(())
}

fn validate_routes(
    server_name: &str,
    route_kind: &str,
    routes: &[String],
    seen: &mut BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    let mut local = BTreeSet::new();
    for route in routes {
        if route.trim().is_empty() {
            return Err(validation(format!(
                "server `{server_name}` {route_kind} entries must not be empty"
            )));
        }
        if !local.insert(route.clone()) {
            return Err(validation(format!(
                "server `{server_name}` declares duplicate {route_kind} `{route}`"
            )));
        }
        if let Some(previous) =
            seen.insert(route.clone(), server_name.to_owned())
        {
            return Err(validation(format!(
                "{route_kind} `{route}` is ambiguous between servers `{previous}` and `{server_name}`"
            )));
        }
    }
    Ok(())
}

fn duration_or_default(
    field: &str,
    value: Option<u64>,
    default: Duration,
) -> Result<Duration, ConfigError> {
    match value {
        Some(0) => {
            Err(validation(format!("{field} must be greater than zero")))
        }
        Some(value) => Ok(Duration::from_millis(value)),
        None => Ok(default),
    }
}

fn toml_to_json(
    value: toml::Value,
    path: &str,
) -> Result<JsonValue, ConfigError> {
    match value {
        toml::Value::String(value) => Ok(JsonValue::String(value)),
        toml::Value::Integer(value) => Ok(JsonValue::Number(value.into())),
        toml::Value::Float(value) => Number::from_f64(value)
            .map(JsonValue::Number)
            .ok_or_else(|| {
                validation(format!(
                    "{path} contains a non-finite float that cannot be represented as JSON"
                ))
            }),
        toml::Value::Boolean(value) => Ok(JsonValue::Bool(value)),
        toml::Value::Datetime(_) => Err(validation(format!(
            "{path} contains a TOML datetime that cannot be represented as JSON"
        ))),
        toml::Value::Array(values) => values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                toml_to_json(value, &format!("{path}[{index}]"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
        toml::Value::Table(values) => {
            let mut object = JsonMap::new();
            for (key, value) in values {
                object.insert(
                    key.clone(),
                    toml_to_json(value, &format!("{path}.{key}"))?,
                );
            }
            Ok(JsonValue::Object(object))
        }
    }
}

fn validation(message: impl Into<String>) -> ConfigError {
    ConfigError::Validation(message.into())
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, time::Duration};

    use serde_json::json;

    use super::{Config, ConfigError};

    const VALID_CONFIG: &str = r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
args = ["--log-file", "/tmp/rust-analyzer.log"]
language_ids = ["rust"]
file_patterns = ["**/*.rs"]

[servers.environment]
RUST_LOG = "info"

[servers.initialization_options]
checkOnSave = true

[servers.initialization_options.cargo]
allFeatures = true
features = ["serde", "toml"]

[servers.timeouts]
request_ms = 12000
shutdown_ms = 3000
"#;

    #[test]
    fn parses_valid_language_server_config() {
        let config = Config::from_toml_str(VALID_CONFIG).unwrap();
        let server = config.server("rust-analyzer").unwrap();

        assert_eq!(config.servers().len(), 1);
        assert_eq!(server.name(), "rust-analyzer");
        assert_eq!(server.command(), "rust-analyzer");
        assert_eq!(
            server.args(),
            &["--log-file".to_owned(), "/tmp/rust-analyzer.log".to_owned()]
        );
        assert_eq!(
            server.environment().get("RUST_LOG").map(String::as_str),
            Some("info")
        );
        assert_eq!(server.language_ids(), &["rust".to_owned()]);
        assert_eq!(server.file_patterns(), &["**/*.rs".to_owned()]);
        assert_eq!(
            server.initialization_options(),
            &json!({
                "checkOnSave": true,
                "cargo": {
                    "allFeatures": true,
                    "features": ["serde", "toml"],
                },
            })
        );
        assert_eq!(server.timeouts().request(), Duration::from_millis(12000));
        assert_eq!(server.timeouts().shutdown(), Duration::from_millis(3000));
    }

    #[test]
    fn builds_a_direct_tokio_command() {
        let config = Config::from_toml_str(VALID_CONFIG).unwrap();
        let command = config.server("rust-analyzer").unwrap().to_command();
        let command = command.as_std();

        assert_eq!(command.get_program(), OsStr::new("rust-analyzer"));
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["--log-file", "/tmp/rust-analyzer.log"]
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == OsStr::new("RUST_LOG") && value == Some(OsStr::new("info"))
        }));
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = parse_error(
            r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
language_ids = ["rust"]
shell = true
"#,
        );

        assert!(error.contains("unknown field"));
        assert!(error.contains("shell"));
    }

    #[test]
    fn rejects_duplicate_server_names() {
        let error = parse_error(
            r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
language_ids = ["rust"]

[[servers]]
name = "rust-analyzer"
command = "rust-analyzer-nightly"
language_ids = ["rust-nightly"]
"#,
        );

        assert!(error.contains("duplicate server name `rust-analyzer`"));
    }

    #[test]
    fn rejects_empty_commands() {
        let error = parse_error(
            r#"
[[servers]]
name = "rust-analyzer"
command = " "
language_ids = ["rust"]
"#,
        );

        assert!(error.contains("command must not be empty"));
    }

    #[test]
    fn rejects_missing_routing_selectors() {
        let error = parse_error(
            r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
"#,
        );

        assert!(error.contains(
            "server `rust-analyzer` must declare language_ids, file_patterns, or both"
        ));
    }

    #[test]
    fn rejects_ambiguous_language_routes() {
        let error = parse_error(
            r#"
[[servers]]
name = "first"
command = "server-one"
language_ids = ["rust"]

[[servers]]
name = "second"
command = "server-two"
language_ids = ["rust"]
"#,
        );

        assert!(error.contains(
            "language id `rust` is ambiguous between servers `first` and `second`"
        ));
    }

    #[test]
    fn rejects_invalid_timeout_values() {
        let error = parse_error(
            r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
language_ids = ["rust"]

[servers.timeouts]
request_ms = 0
"#,
        );

        assert!(error.contains("request_ms must be greater than zero"));
    }

    #[test]
    fn rejects_initialization_options_that_are_not_json() {
        let error = parse_error(
            r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
language_ids = ["rust"]

[servers.initialization_options]
generated_at = 2026-09-05T14:43:00Z
"#,
        );

        assert!(error.contains(
            "servers.rust-analyzer.initialization_options.generated_at contains a TOML datetime"
        ));
    }

    #[test]
    fn uses_default_timeout_and_initialization_options() {
        let config = Config::from_toml_str(
            r#"
[[servers]]
name = "pyright"
command = "pyright-langserver"
args = ["--stdio"]
file_patterns = ["**/*.py"]
"#,
        )
        .unwrap();
        let server = config.server("pyright").unwrap();

        assert_eq!(server.timeouts().request(), Duration::from_secs(30));
        assert_eq!(server.timeouts().shutdown(), Duration::from_secs(5));
        assert_eq!(server.initialization_options(), &json!({}));
    }

    fn parse_error(input: &str) -> String {
        match Config::from_toml_str(input) {
            Ok(_) => panic!("config should have failed validation"),
            Err(ConfigError::Validation(message)) => message,
            Err(error) => error.to_string(),
        }
    }
}
