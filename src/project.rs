use std::{
    env,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::{
    cli::CliOptions,
    config::{Config, ConfigError},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    root: PathBuf,
    config_path: Option<PathBuf>,
}

impl Project {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StartupState {
    project: Project,
    config: Option<Config>,
}

impl StartupState {
    pub fn from_options(options: CliOptions) -> Result<Self, ProjectError> {
        let cwd = env::current_dir().map_err(ProjectError::CurrentDirectory)?;
        Self::from_options_in(options, cwd)
    }

    pub fn from_options_in(
        options: CliOptions,
        cwd: impl AsRef<Path>,
    ) -> Result<Self, ProjectError> {
        let cwd = cwd.as_ref();
        let root_input = options
            .root()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf());
        let root_path =
            canonicalize_root(resolve_against_cwd(&root_input, cwd))?;

        let config_path = options
            .config_path()
            .map(|path| canonicalize_config(resolve_against_cwd(path, cwd)))
            .transpose()?;
        let config = config_path
            .as_ref()
            .map(Config::from_path)
            .transpose()
            .map_err(ProjectError::Config)?;

        Ok(Self {
            project: Project {
                root: root_path,
                config_path,
            },
            config,
        })
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn config(&self) -> Option<&Config> {
        self.config.as_ref()
    }
}

#[derive(Debug)]
pub enum ProjectError {
    CurrentDirectory(io::Error),
    Root { path: PathBuf, source: io::Error },
    ConfigPath { path: PathBuf, source: io::Error },
    Config(ConfigError),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDirectory(source) => {
                write!(formatter, "failed to read current directory: {source}")
            }
            Self::Root { path, source } => {
                write!(
                    formatter,
                    "failed to resolve project root `{}`: {source}",
                    path.display()
                )
            }
            Self::ConfigPath { path, source } => {
                write!(
                    formatter,
                    "failed to resolve config `{}`: {source}",
                    path.display()
                )
            }
            Self::Config(source) => write!(formatter, "{source}"),
        }
    }
}

impl Error for ProjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CurrentDirectory(source)
            | Self::Root { source, .. }
            | Self::ConfigPath { source, .. } => Some(source),
            Self::Config(source) => Some(source),
        }
    }
}

fn resolve_against_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn canonicalize_root(path: PathBuf) -> Result<PathBuf, ProjectError> {
    fs::canonicalize(&path)
        .map_err(|source| ProjectError::Root { path, source })
}

fn canonicalize_config(path: PathBuf) -> Result<PathBuf, ProjectError> {
    fs::canonicalize(&path)
        .map_err(|source| ProjectError::ConfigPath { path, source })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::StartupState;
    use crate::cli::CliOptions;

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    const VALID_CONFIG: &str = r#"
[[servers]]
name = "rust-analyzer"
command = "rust-analyzer"
language_ids = ["rust"]
"#;

    #[test]
    fn defaults_root_to_current_directory() {
        let cwd = unique_dir("default-root");
        let startup =
            StartupState::from_options_in(CliOptions::default(), &cwd).unwrap();

        assert_eq!(startup.project().root(), canonical(&cwd).as_path());
        assert_eq!(startup.project().config_path(), None);
        assert!(startup.config().is_none());
    }

    #[test]
    fn canonicalizes_explicit_root_once() {
        let root = unique_dir("explicit-root");
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        let options = CliOptions::new(None, Some(nested.join("..")));

        let startup = StartupState::from_options_in(options, &root).unwrap();

        assert_eq!(startup.project().root(), canonical(&root).as_path());
    }

    #[test]
    fn resolves_and_loads_relative_config_path() {
        let cwd = unique_dir("relative-config");
        let config_path = cwd.join("deixis.toml");
        fs::write(&config_path, VALID_CONFIG).unwrap();
        let options = CliOptions::new(Some("deixis.toml".into()), None);

        let startup = StartupState::from_options_in(options, &cwd).unwrap();

        assert_eq!(
            startup.project().config_path(),
            Some(canonical(&config_path).as_path())
        );
        assert_eq!(startup.config().unwrap().servers().len(), 1);
    }

    #[test]
    fn rejects_missing_config_before_serving() {
        let cwd = unique_dir("missing-config");
        let options = CliOptions::new(Some("missing.toml".into()), None);

        let error = StartupState::from_options_in(options, &cwd).unwrap_err();

        assert!(error.to_string().contains("failed to resolve config"));
        assert!(error.to_string().contains("missing.toml"));
    }

    #[test]
    fn rejects_invalid_config_before_serving() {
        let cwd = unique_dir("invalid-config");
        fs::write(cwd.join("deixis.toml"), "servers = true").unwrap();
        let options = CliOptions::new(Some("deixis.toml".into()), None);

        let error = StartupState::from_options_in(options, &cwd).unwrap_err();

        assert!(error.to_string().contains("failed to parse config TOML"));
    }

    fn unique_dir(name: &str) -> PathBuf {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "deixis-{name}-{}-{nanos}-{sequence}",
            process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn canonical(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap()
    }
}
