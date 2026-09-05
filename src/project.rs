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

    pub fn resolve_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ProjectFile, ProjectPathError> {
        let path = path.as_ref();
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let absolute = fs::canonicalize(&candidate).map_err(|source| {
            ProjectPathError::Resolve {
                path: candidate,
                source,
            }
        })?;
        let relative = absolute
            .strip_prefix(&self.root)
            .map(Path::to_path_buf)
            .map_err(|_| ProjectPathError::OutsideRoot {
                path: absolute.clone(),
                root: self.root.clone(),
            })?;
        let metadata = fs::metadata(&absolute).map_err(|source| {
            ProjectPathError::Resolve {
                path: absolute.clone(),
                source,
            }
        })?;
        if !metadata.is_file() {
            return Err(ProjectPathError::NotFile { path: absolute });
        }

        Ok(ProjectFile { absolute, relative })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    absolute: PathBuf,
    relative: PathBuf,
}

impl ProjectFile {
    pub fn absolute(&self) -> &Path {
        &self.absolute
    }

    pub fn relative(&self) -> &Path {
        &self.relative
    }
}

#[derive(Debug)]
pub enum ProjectPathError {
    Resolve { path: PathBuf, source: io::Error },
    OutsideRoot { path: PathBuf, root: PathBuf },
    NotFile { path: PathBuf },
}

impl fmt::Display for ProjectPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resolve { path, source } => {
                write!(
                    formatter,
                    "failed to resolve project file `{}`: {source}",
                    path.display()
                )
            }
            Self::OutsideRoot { path, root } => {
                write!(
                    formatter,
                    "project file `{}` is outside project root `{}`",
                    path.display(),
                    root.display()
                )
            }
            Self::NotFile { path } => {
                write!(
                    formatter,
                    "project path `{}` is not a regular file",
                    path.display()
                )
            }
        }
    }
}

impl Error for ProjectPathError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Resolve { source, .. } => Some(source),
            Self::OutsideRoot { .. } | Self::NotFile { .. } => None,
        }
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
        fs, io,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{Project, ProjectPathError, StartupState};
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

    #[test]
    fn resolves_relative_and_absolute_project_files() {
        let root = unique_dir("resolve-project-file");
        let source = root.join("src").join("lib.rs");
        fs::create_dir(source.parent().unwrap()).unwrap();
        fs::write(&source, "pub fn answer() -> u8 { 42 }").unwrap();
        let project = project(&root);

        let relative = project.resolve_file("src/lib.rs").unwrap();
        let absolute = project.resolve_file(&source).unwrap();

        assert_eq!(relative.absolute(), canonical(&source));
        assert_eq!(relative.relative(), Path::new("src").join("lib.rs"));
        assert_eq!(absolute, relative);
    }

    #[test]
    fn normalizes_parent_components_that_remain_inside_the_root() {
        let root = unique_dir("normalize-project-file");
        let source = root.join("lib.rs");
        fs::create_dir(root.join("src")).unwrap();
        fs::write(&source, "pub fn answer() -> u8 { 42 }").unwrap();
        let project = project(&root);

        let resolved = project.resolve_file("src/../lib.rs").unwrap();

        assert_eq!(resolved.absolute(), canonical(&source));
        assert_eq!(resolved.relative(), Path::new("lib.rs"));
    }

    #[test]
    fn rejects_relative_traversal_and_absolute_external_paths() {
        let parent = unique_dir("external-project-file");
        let root = parent.join("project");
        let external = parent.join("external.rs");
        fs::create_dir(&root).unwrap();
        fs::write(&external, "secret").unwrap();
        let project = project(&root);

        let traversal = project.resolve_file("../external.rs").unwrap_err();
        let absolute = project.resolve_file(&external).unwrap_err();

        assert!(matches!(traversal, ProjectPathError::OutsideRoot { .. }));
        assert!(matches!(absolute, ProjectPathError::OutsideRoot { .. }));
    }

    #[test]
    fn rejects_a_sibling_with_the_same_textual_prefix_as_the_root() {
        let parent = unique_dir("prefix-project-file");
        let root = parent.join("project");
        let sibling = parent.join("project-sibling");
        let external = sibling.join("lib.rs");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&sibling).unwrap();
        fs::write(&external, "secret").unwrap();
        let project = project(&root);

        let error = project.resolve_file(&external).unwrap_err();

        assert!(matches!(error, ProjectPathError::OutsideRoot { .. }));
    }

    #[test]
    fn rejects_missing_paths_and_non_file_targets() {
        let root = unique_dir("invalid-project-file");
        let project = project(&root);

        let missing = project.resolve_file("missing.rs").unwrap_err();
        let directory = project.resolve_file(".").unwrap_err();

        assert!(matches!(missing, ProjectPathError::Resolve { .. }));
        assert!(matches!(directory, ProjectPathError::NotFile { .. }));
    }

    #[test]
    fn accepts_an_internal_file_symlink_as_its_canonical_target() {
        let root = unique_dir("internal-symlink");
        let source = root.join("source.rs");
        let alias = root.join("alias.rs");
        fs::write(&source, "pub fn answer() -> u8 { 42 }").unwrap();
        if !create_file_symlink_or_skip(&source, &alias) {
            return;
        }
        let project = project(&root);

        let resolved = project.resolve_file("alias.rs").unwrap();

        assert_eq!(resolved.absolute(), canonical(&source));
        assert_eq!(resolved.relative(), Path::new("source.rs"));
    }

    #[test]
    fn rejects_an_external_target_through_a_directory_symlink() {
        let parent = unique_dir("external-symlink");
        let root = parent.join("project");
        let external = parent.join("external");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&external).unwrap();
        fs::write(external.join("secret.rs"), "secret").unwrap();
        if !create_directory_symlink_or_skip(&external, &root.join("link")) {
            return;
        }
        let project = project(&root);

        let error = project.resolve_file("link/secret.rs").unwrap_err();

        assert!(matches!(error, ProjectPathError::OutsideRoot { .. }));
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

    fn project(root: &Path) -> Project {
        Project {
            root: canonical(root),
            config_path: None,
        }
    }

    #[cfg(unix)]
    fn create_file_symlink(
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        std::os::unix::fs::symlink(source, destination)
    }

    #[cfg(windows)]
    fn create_file_symlink(
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        std::os::windows::fs::symlink_file(source, destination)
    }

    #[cfg(unix)]
    fn create_directory_symlink(
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        std::os::unix::fs::symlink(source, destination)
    }

    #[cfg(windows)]
    fn create_directory_symlink(
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        std::os::windows::fs::symlink_dir(source, destination)
    }

    fn create_file_symlink_or_skip(source: &Path, destination: &Path) -> bool {
        symlink_or_skip(create_file_symlink(source, destination))
    }

    fn create_directory_symlink_or_skip(
        source: &Path,
        destination: &Path,
    ) -> bool {
        symlink_or_skip(create_directory_symlink(source, destination))
    }

    fn symlink_or_skip(result: io::Result<()>) -> bool {
        match result {
            Ok(()) => true,
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(1314) => false,
            Err(error) => panic!("failed to create test symlink: {error}"),
        }
    }
}
