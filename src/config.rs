use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use globset::Glob;
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

    pub fn route<'a>(
        &'a self,
        path: &Path,
        server_name: Option<&str>,
    ) -> Result<ServerRoute<'a>, ConfigRouteError> {
        if let Some(server_name) = server_name {
            let server = self.server(server_name).ok_or_else(|| {
                ConfigRouteError::UnknownServer(server_name.to_owned())
            })?;
            return server
                .language_id_for_path(path)?
                .map(|language_id| ServerRoute {
                    server,
                    language_id,
                })
                .ok_or_else(|| ConfigRouteError::NoMatch {
                    path: path.to_path_buf(),
                    server: Some(server_name.to_owned()),
                });
        }

        let mut matches = Vec::new();
        for server in &self.servers {
            if let Some(language_id) = server.language_id_for_path(path)? {
                matches.push(ServerRoute {
                    server,
                    language_id,
                });
            }
        }

        match matches.len() {
            0 => Err(ConfigRouteError::NoMatch {
                path: path.to_path_buf(),
                server: None,
            }),
            1 => Ok(matches.remove(0)),
            _ => Err(ConfigRouteError::AmbiguousServers {
                path: path.to_path_buf(),
                servers: matches
                    .into_iter()
                    .map(|route| route.server.name.clone())
                    .collect(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ServerRoute<'a> {
    server: &'a LanguageServerConfig,
    language_id: &'a str,
}

impl<'a> ServerRoute<'a> {
    pub fn server(self) -> &'a LanguageServerConfig {
        self.server
    }

    pub fn language_id(self) -> &'a str {
        self.language_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigRouteError {
    UnknownServer(String),
    NoMatch {
        path: PathBuf,
        server: Option<String>,
    },
    AmbiguousServers {
        path: PathBuf,
        servers: Vec<String>,
    },
    AmbiguousLanguages {
        path: PathBuf,
        server: String,
        languages: Vec<String>,
    },
}

impl fmt::Display for ConfigRouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownServer(server) => {
                write!(formatter, "server `{server}` is not configured")
            }
            Self::NoMatch {
                path,
                server: Some(server),
            } => write!(
                formatter,
                "server `{server}` has no route for file `{}`",
                path.display()
            ),
            Self::NoMatch { path, server: None } => write!(
                formatter,
                "no language server matches file `{}`",
                path.display()
            ),
            Self::AmbiguousServers { path, servers } => write!(
                formatter,
                "file `{}` matches multiple servers: {}",
                path.display(),
                quoted_list(servers)
            ),
            Self::AmbiguousLanguages {
                path,
                server,
                languages,
            } => write!(
                formatter,
                "file `{}` matches multiple language IDs for server `{server}`: {}",
                path.display(),
                quoted_list(languages)
            ),
        }
    }
}

impl Error for ConfigRouteError {}

#[derive(Debug, Clone, PartialEq)]
pub struct LanguageServerConfig {
    name: String,
    command: String,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
    file_extensions: BTreeMap<String, String>,
    file_patterns: BTreeMap<String, String>,
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

    pub fn file_extensions(&self) -> &BTreeMap<String, String> {
        &self.file_extensions
    }

    pub fn file_patterns(&self) -> &BTreeMap<String, String> {
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

    fn language_id_for_path<'a>(
        &'a self,
        path: &Path,
    ) -> Result<Option<&'a str>, ConfigRouteError> {
        let pattern_languages = self
            .file_patterns
            .iter()
            .filter(|(pattern, _)| {
                Glob::new(pattern)
                    .expect("validated file pattern should compile")
                    .compile_matcher()
                    .is_match(path)
            })
            .map(|(_, language_id)| language_id.as_str())
            .collect::<BTreeSet<_>>();
        if pattern_languages.len() > 1 {
            return Err(ConfigRouteError::AmbiguousLanguages {
                path: path.to_path_buf(),
                server: self.name.clone(),
                languages: pattern_languages
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            });
        }
        if let Some(language_id) = pattern_languages.into_iter().next() {
            return Ok(Some(language_id));
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str())
        else {
            return Ok(None);
        };
        Ok(self
            .file_extensions
            .iter()
            .filter(|(extension, _)| file_name.ends_with(extension.as_str()))
            .max_by_key(|(extension, _)| extension.len())
            .map(|(_, language_id)| language_id.as_str()))
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
    servers: BTreeMap<String, RawLanguageServerConfig>,
}

impl RawConfig {
    fn validate(self) -> Result<Config, ConfigError> {
        if self.servers.is_empty() {
            return Err(validation("config must declare at least one server"));
        }

        let mut servers = Vec::with_capacity(self.servers.len());

        for (name, server) in self.servers {
            let name = require_non_empty("server name", name)?;
            let command = require_non_empty(
                &format!("server `{name}` command"),
                server.command,
            )?;
            validate_environment(&name, &server.environment)?;
            validate_file_extensions(&name, &server.file_extensions)?;
            validate_file_patterns(&name, &server.file_patterns)?;
            if server.file_extensions.is_empty()
                && server.file_patterns.is_empty()
            {
                return Err(validation(format!(
                    "server `{name}` must declare file_extensions, file_patterns, or both"
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
                file_extensions: server.file_extensions,
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
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    file_extensions: BTreeMap<String, String>,
    #[serde(default)]
    file_patterns: BTreeMap<String, String>,
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

fn validate_file_extensions(
    server_name: &str,
    routes: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    for (extension, language_id) in routes {
        if !extension.starts_with('.') {
            return Err(validation(format!(
                "server `{server_name}` extension `{extension}` must start with `.`"
            )));
        }
        if extension.len() == 1
            || extension.contains('/')
            || extension.contains('\\')
        {
            return Err(validation(format!(
                "server `{server_name}` extension `{extension}` must be a file-name suffix"
            )));
        }
        if language_id.trim().is_empty() {
            return Err(validation(format!(
                "server `{server_name}` language id for extension `{extension}` must not be empty"
            )));
        }
    }
    Ok(())
}

fn validate_file_patterns(
    server_name: &str,
    routes: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    for (pattern, language_id) in routes {
        if pattern.trim().is_empty() {
            return Err(validation(format!(
                "server `{server_name}` file patterns must not be empty"
            )));
        }
        if let Err(source) = Glob::new(pattern) {
            return Err(validation(format!(
                "server `{server_name}` has invalid file pattern `{pattern}`: {source}"
            )));
        }
        if language_id.trim().is_empty() {
            return Err(validation(format!(
                "server `{server_name}` language id for file pattern `{pattern}` must not be empty"
            )));
        }
    }
    Ok(())
}

fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("`{value}`"))
        .collect::<Vec<_>>()
        .join(", ")
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
    use std::{ffi::OsStr, path::Path, time::Duration};

    use serde_json::json;

    use super::{Config, ConfigError};

    const VALID_CONFIG: &str = r#"
[servers.rust]
command = "rust-analyzer"
args = ["--log-file", "/tmp/rust-analyzer.log"]

[servers.rust.file_extensions]
".rs" = "rust"

[servers.rust.file_patterns]
"generated/**/*.rs" = "rust-generated"

[servers.rust.environment]
RUST_LOG = "info"

[servers.rust.initialization_options]
checkOnSave = true

[servers.rust.initialization_options.cargo]
allFeatures = true
features = ["serde", "toml"]

[servers.rust.timeouts]
request_ms = 12000
shutdown_ms = 3000
"#;

    #[test]
    fn parses_valid_language_server_config() {
        let config = Config::from_toml_str(VALID_CONFIG).unwrap();
        let server = config.server("rust").unwrap();

        assert_eq!(config.servers().len(), 1);
        assert_eq!(server.name(), "rust");
        assert_eq!(server.command(), "rust-analyzer");
        assert_eq!(
            server.args(),
            &["--log-file".to_owned(), "/tmp/rust-analyzer.log".to_owned()]
        );
        assert_eq!(
            server.environment().get("RUST_LOG").map(String::as_str),
            Some("info")
        );
        assert_eq!(
            server.file_extensions().get(".rs").map(String::as_str),
            Some("rust")
        );
        assert_eq!(
            server
                .file_patterns()
                .get("generated/**/*.rs")
                .map(String::as_str),
            Some("rust-generated")
        );
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
        let command = config.server("rust").unwrap().to_command();
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
[servers.rust]
command = "rust-analyzer"
file_extensions = { ".rs" = "rust" }
shell = true
"#,
        );

        assert!(error.contains("unknown field"));
        assert!(error.contains("shell"));
    }

    #[test]
    fn rejects_duplicate_server_tables() {
        let error = parse_error(
            r#"
[servers.rust]
command = "rust-analyzer"
file_extensions = { ".rs" = "rust" }

[servers.rust]
command = "rust-analyzer-nightly"
file_extensions = { ".rs" = "rust" }
"#,
        );

        assert!(error.contains("duplicate key"));
    }

    #[test]
    fn rejects_empty_commands() {
        let error = parse_error(
            r#"
[servers.rust]
command = " "
file_extensions = { ".rs" = "rust" }
"#,
        );

        assert!(error.contains("command must not be empty"));
    }

    #[test]
    fn rejects_missing_routing_selectors() {
        let error = parse_error(
            r#"
[servers.rust]
command = "rust-analyzer"
"#,
        );

        assert!(error.contains(
            "server `rust` must declare file_extensions, file_patterns, or both"
        ));
    }

    #[test]
    fn reports_ambiguous_routes_and_accepts_an_explicit_server() {
        let config = Config::from_toml_str(
            r#"
[servers.first]
command = "server-one"
file_extensions = { ".rs" = "rust" }

[servers.second]
command = "server-two"
file_extensions = { ".rs" = "rust" }
"#,
        )
        .unwrap();

        let error = config.route(Path::new("src/lib.rs"), None).unwrap_err();
        let route = config
            .route(Path::new("src/lib.rs"), Some("second"))
            .unwrap();

        assert!(error.to_string().contains(
            "file `src/lib.rs` matches multiple servers: `first`, `second`"
        ));
        assert_eq!(route.server().name(), "second");
        assert_eq!(route.language_id(), "rust");
    }

    #[test]
    fn routes_by_longest_extension_and_prefers_file_patterns() {
        let config = Config::from_toml_str(
            r#"
[servers.typescript]
command = "vtsls"
file_extensions = { ".ts" = "typescript", ".d.ts" = "typescript-declaration" }
file_patterns = { "generated/**/*.d.ts" = "generated-typescript" }
"#,
        )
        .unwrap();

        let declaration =
            config.route(Path::new("src/types.d.ts"), None).unwrap();
        let generated = config
            .route(Path::new("generated/api/types.d.ts"), None)
            .unwrap();

        assert_eq!(declaration.language_id(), "typescript-declaration");
        assert_eq!(generated.language_id(), "generated-typescript");
    }

    #[test]
    fn reports_unknown_servers_and_unmatched_files() {
        let config = Config::from_toml_str(VALID_CONFIG).unwrap();

        assert!(
            config
                .route(Path::new("src/lib.rs"), Some("missing"))
                .unwrap_err()
                .to_string()
                .contains("server `missing` is not configured")
        );
        assert!(
            config
                .route(Path::new("README.md"), None)
                .unwrap_err()
                .to_string()
                .contains("no language server matches file `README.md`")
        );
    }

    #[test]
    fn rejects_invalid_timeout_values() {
        let error = parse_error(
            r#"
[servers.rust]
command = "rust-analyzer"
file_extensions = { ".rs" = "rust" }

[servers.rust.timeouts]
request_ms = 0
"#,
        );

        assert!(error.contains("request_ms must be greater than zero"));
    }

    #[test]
    fn rejects_initialization_options_that_are_not_json() {
        let error = parse_error(
            r#"
[servers.rust]
command = "rust-analyzer"
file_extensions = { ".rs" = "rust" }

[servers.rust.initialization_options]
generated_at = 2026-09-05T14:43:00Z
"#,
        );

        assert!(error.contains(
            "servers.rust.initialization_options.generated_at contains a TOML datetime"
        ));
    }

    #[test]
    fn uses_default_timeout_and_initialization_options() {
        let config = Config::from_toml_str(
            r#"
[servers.pyright]
command = "pyright-langserver"
args = ["--stdio"]
file_extensions = { ".py" = "python" }
"#,
        )
        .unwrap();
        let server = config.server("pyright").unwrap();

        assert_eq!(server.timeouts().request(), Duration::from_secs(30));
        assert_eq!(server.timeouts().shutdown(), Duration::from_secs(5));
        assert_eq!(server.initialization_options(), &json!({}));
    }

    #[test]
    fn rejects_invalid_extensions_patterns_and_language_ids() {
        let invalid_extension = parse_error(
            r#"
[servers.rust]
command = "rust-analyzer"
file_extensions = { "rs" = "rust" }
"#,
        );
        let invalid_pattern = parse_error(
            r#"
[servers.rust]
command = "rust-analyzer"
file_patterns = { "[" = "rust" }
"#,
        );
        let empty_language = parse_error(
            r#"
[servers.rust]
command = "rust-analyzer"
file_extensions = { ".rs" = " " }
"#,
        );

        assert!(
            invalid_extension.contains("extension `rs` must start with `.`")
        );
        assert!(invalid_pattern.contains("invalid file pattern `[`"));
        assert!(
            empty_language
                .contains("language id for extension `.rs` must not be empty")
        );
    }

    fn parse_error(input: &str) -> String {
        match Config::from_toml_str(input) {
            Ok(_) => panic!("config should have failed validation"),
            Err(ConfigError::Validation(message)) => message,
            Err(error) => error.to_string(),
        }
    }
}
