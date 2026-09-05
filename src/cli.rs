use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOptions {
    config_path: Option<PathBuf>,
    root: Option<PathBuf>,
}

impl CliOptions {
    pub fn new(config_path: Option<PathBuf>, root: Option<PathBuf>) -> Self {
        Self { config_path, root }
    }

    pub fn parse_env() -> Result<Self, CliError> {
        Self::parse_from(env::args_os())
    }

    pub fn parse_from<I, S>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut args = args.into_iter().map(Into::into);
        let _program = args.next();
        let mut options = Self::default();

        while let Some(arg) = args.next() {
            if arg.as_os_str() == OsStr::new("--config") {
                let value = next_value(&mut args, "--config")?;
                set_path(&mut options.config_path, "--config", value)?;
            } else if let Some(value) =
                inline_value(arg.as_os_str(), "--config=")
            {
                set_path(&mut options.config_path, "--config", value)?;
            } else if arg.as_os_str() == OsStr::new("--root") {
                let value = next_value(&mut args, "--root")?;
                set_path(&mut options.root, "--root", value)?;
            } else if let Some(value) = inline_value(arg.as_os_str(), "--root=")
            {
                set_path(&mut options.root, "--root", value)?;
            } else {
                return Err(CliError::UnknownArgument(arg));
            }
        }

        Ok(options)
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CliError {
    MissingValue(&'static str),
    DuplicateOption(&'static str),
    UnknownArgument(OsString),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(option) => {
                write!(formatter, "missing value for `{option}`")
            }
            Self::DuplicateOption(option) => {
                write!(formatter, "duplicate option `{option}`")
            }
            Self::UnknownArgument(argument) => write!(
                formatter,
                "unknown argument `{}`",
                argument.to_string_lossy()
            ),
        }
    }
}

impl Error for CliError {}

fn next_value(
    args: &mut impl Iterator<Item = OsString>,
    option: &'static str,
) -> Result<OsString, CliError> {
    let value = args.next().ok_or(CliError::MissingValue(option))?;
    if value.is_empty() || looks_like_option(value.as_os_str()) {
        return Err(CliError::MissingValue(option));
    }
    Ok(value)
}

fn inline_value(argument: &OsStr, prefix: &str) -> Option<OsString> {
    argument
        .to_str()
        .and_then(|argument| argument.strip_prefix(prefix))
        .map(OsString::from)
}

fn set_path(
    slot: &mut Option<PathBuf>,
    option: &'static str,
    value: OsString,
) -> Result<(), CliError> {
    if value.is_empty() {
        return Err(CliError::MissingValue(option));
    }
    if slot.is_some() {
        return Err(CliError::DuplicateOption(option));
    }
    *slot = Some(PathBuf::from(value));
    Ok(())
}

fn looks_like_option(value: &OsStr) -> bool {
    value.to_str().is_some_and(|value| value.starts_with("--"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{CliError, CliOptions};

    #[test]
    fn parses_config_and_root_options() {
        let options = CliOptions::parse_from([
            "deixis",
            "--config",
            "deixis.toml",
            "--root=/workspace/project",
        ])
        .unwrap();

        assert_eq!(options.config_path(), Some(Path::new("deixis.toml")));
        assert_eq!(options.root(), Some(Path::new("/workspace/project")));
    }

    #[test]
    fn rejects_missing_option_values() {
        let error = CliOptions::parse_from(["deixis", "--config"]).unwrap_err();

        assert_eq!(error, CliError::MissingValue("--config"));
    }

    #[test]
    fn rejects_unknown_arguments() {
        let error = CliOptions::parse_from(["deixis", "--workspace", "root"])
            .unwrap_err();

        assert_eq!(error.to_string(), "unknown argument `--workspace`");
    }

    #[test]
    fn rejects_duplicate_options() {
        let error =
            CliOptions::parse_from(["deixis", "--root", ".", "--root", ".."])
                .unwrap_err();

        assert_eq!(error, CliError::DuplicateOption("--root"));
    }
}
