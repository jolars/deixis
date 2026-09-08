use std::{
    env,
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

static MOCK_LSP_SERVER: OnceLock<Result<PathBuf, String>> = OnceLock::new();

pub fn mock_lsp_server() -> Result<PathBuf, Box<dyn Error>> {
    MOCK_LSP_SERVER
        .get_or_init(compile_mock_lsp_server)
        .clone()
        .map_err(Into::into)
}

pub fn unique_dir(name: &str) -> Result<PathBuf, std::io::Error> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = env::temp_dir()
        .join(format!("deixis-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn toml_string(value: &Path) -> String {
    serde_json::to_string(&value.to_string_lossy()).unwrap()
}

fn compile_mock_lsp_server() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = manifest_dir.join("tests/fixtures/mock_lsp_server.rs");
    let output = unique_dir("mock-lsp-build")
        .map_err(|error| error.to_string())?
        .join(format!("mock-lsp{}", env::consts::EXE_SUFFIX));

    let status = Command::new("rustc")
        .arg("--edition=2024")
        .arg(&source)
        .arg("-o")
        .arg(&output)
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| {
            format!(
                "failed to compile mock LSP server `{}`: {error}",
                source.display()
            )
        })?;

    if !status.success() {
        return Err(format!(
            "mock LSP server compilation failed with status {status}"
        ));
    }

    Ok(output)
}
