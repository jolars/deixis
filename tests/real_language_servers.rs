use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use deixis::{
    cli::CliOptions,
    lsp::{Hover, LazyLanguageServer},
    positions::Position,
    project::StartupState,
};
use tokio::time::{Instant, sleep};

const STARTUP_TIMEOUT_MS: u64 = 30_000;
const REQUEST_TIMEOUT_MS: u64 = 30_000;
const SHUTDOWN_TIMEOUT_MS: u64 = 5_000;
const RESULT_TIMEOUT: Duration = Duration::from_secs(15);
const RESULT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

#[tokio::test]
#[ignore = "requires typescript-language-server"]
async fn works_with_typescript_language_server() -> Result<(), Box<dyn Error>> {
    assert_real_server_hover(CompatibilityCase {
        name: "typescript-language-server",
        command_env: "DEIXIS_TYPESCRIPT_LANGUAGE_SERVER",
        command: "typescript-language-server",
        args: &["--stdio"],
        extension: ".ts",
        language_id: "typescript",
        file_name: "main.ts",
        source: r#"export function double(value: number): number {
    return value * 2;
}

const result = double(21);
console.log(result);
"#,
        hover_target: "double",
        project_files: &[(
            "tsconfig.json",
            r#"{"compilerOptions":{"strict":true},"files":["main.ts"]}"#,
        )],
        initialization_options: "disableAutomaticTypingAcquisition = true",
    })
    .await
}

#[tokio::test]
#[ignore = "requires pyright-langserver"]
async fn works_with_pyright() -> Result<(), Box<dyn Error>> {
    assert_real_server_hover(CompatibilityCase {
        name: "pyright",
        command_env: "DEIXIS_PYRIGHT_LANGSERVER",
        command: "pyright-langserver",
        args: &["--stdio"],
        extension: ".py",
        language_id: "python",
        file_name: "main.py",
        source: r#"def double(value: int) -> int:
    return value * 2


result = double(21)
print(result)
"#,
        hover_target: "double",
        project_files: &[(
            "pyrightconfig.json",
            r#"{"include":["main.py"],"typeCheckingMode":"strict"}"#,
        )],
        initialization_options: "",
    })
    .await
}

#[tokio::test]
#[ignore = "requires gopls"]
async fn works_with_gopls() -> Result<(), Box<dyn Error>> {
    assert_real_server_hover(CompatibilityCase {
        name: "gopls",
        command_env: "DEIXIS_GOPLS",
        command: "gopls",
        args: &[],
        extension: ".go",
        language_id: "go",
        file_name: "main.go",
        source: r#"package main

import "fmt"

func double(value int) int {
	return value * 2
}

func main() {
	result := double(21)
	fmt.Println(result)
}
"#,
        hover_target: "double",
        project_files: &[(
            "go.mod",
            "module example.com/deixis-compatibility\n\ngo 1.23\n",
        )],
        initialization_options: "",
    })
    .await
}

#[tokio::test]
#[ignore = "requires clangd"]
async fn works_with_clangd() -> Result<(), Box<dyn Error>> {
    assert_real_server_hover(CompatibilityCase {
        name: "clangd",
        command_env: "DEIXIS_CLANGD",
        command: "clangd",
        args: &[],
        extension: ".cc",
        language_id: "cpp",
        file_name: "main.cc",
        source: r#"int double_value(int value) {
    return value * 2;
}

int main() {
    return double_value(21);
}
"#,
        hover_target: "double_value",
        project_files: &[("compile_flags.txt", "-xc++\n-std=c++17\n-Wall\n")],
        initialization_options: "",
    })
    .await
}

#[tokio::test]
#[ignore = "requires deno"]
async fn works_with_deno_initialization_options() -> Result<(), Box<dyn Error>>
{
    assert_real_server_hover(CompatibilityCase {
        name: "deno",
        command_env: "DEIXIS_DENO",
        command: "deno",
        args: &["lsp"],
        extension: ".ts",
        language_id: "typescript",
        file_name: "main.ts",
        source: r#"export function double(value: number): number {
    return value * 2;
}

const result = double(21);
console.log(result);
"#,
        hover_target: "double",
        project_files: &[(
            "deno.json",
            r#"{"compilerOptions":{"strict":true}}"#,
        )],
        initialization_options: "enable = true",
    })
    .await
}

struct CompatibilityCase {
    name: &'static str,
    command_env: &'static str,
    command: &'static str,
    args: &'static [&'static str],
    extension: &'static str,
    language_id: &'static str,
    file_name: &'static str,
    source: &'static str,
    hover_target: &'static str,
    project_files: &'static [(&'static str, &'static str)],
    initialization_options: &'static str,
}

async fn assert_real_server_hover(
    case: CompatibilityCase,
) -> Result<(), Box<dyn Error>> {
    let root = unique_dir(case.name)?;
    fs::write(root.join(case.file_name), case.source)?;
    for (path, contents) in case.project_files {
        fs::write(root.join(path), contents)?;
    }

    let command = env::var_os(case.command_env)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(case.command));
    let config_path = write_config(&root, &command, &case)?;
    let startup = StartupState::from_options_in(
        CliOptions::new(Some(config_path), Some(root.clone())),
        &root,
    )?;
    let config = startup
        .config()
        .expect("compatibility config should be loaded")
        .servers()[0]
        .clone();
    let manager = LazyLanguageServer::new(config, startup.project().clone());

    let hover_result = hover_until_ready(&manager, &case).await;
    let shutdown_result = manager.shutdown().await;

    let hover = hover_result?;
    let shutdown = shutdown_result?;
    assert!(shutdown.started(), "{} never started", case.name);
    assert!(
        !shutdown.forced(),
        "{} required forced termination",
        case.name
    );
    let exit_status = shutdown
        .exit_status()
        .expect("a started language server should have an exit status");
    assert!(
        exit_status.success(),
        "{} exited with {exit_status}",
        case.name
    );
    assert!(
        !hover.text().is_empty(),
        "{} returned an empty hover",
        case.name
    );
    Ok(())
}

async fn hover_until_ready(
    manager: &LazyLanguageServer,
    case: &CompatibilityCase,
) -> Result<Hover, Box<dyn Error>> {
    let position = last_position(case.source, case.hover_target)?;
    let deadline = Instant::now() + RESULT_TIMEOUT;

    loop {
        if let Some(hover) = manager
            .hover(case.file_name, case.language_id, position)
            .await?
        {
            return Ok(hover);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "{} returned no hover within {RESULT_TIMEOUT:?}",
                case.name
            ))
            .into());
        }
        sleep(RESULT_RETRY_INTERVAL).await;
    }
}

fn last_position(source: &str, target: &str) -> Result<Position, io::Error> {
    let byte_offset = source.rfind(target).ok_or_else(|| {
        io::Error::other(format!("test source does not contain `{target}`"))
    })?;
    let preceding = &source[..byte_offset];
    let line = preceding.bytes().filter(|byte| *byte == b'\n').count();
    let character = preceding
        .rsplit_once('\n')
        .map_or(preceding.len(), |(_, line)| line.len());

    Ok(Position::new(
        u32::try_from(line).map_err(io::Error::other)?,
        u32::try_from(character).map_err(io::Error::other)?,
    ))
}

fn write_config(
    root: &Path,
    command: &Path,
    case: &CompatibilityCase,
) -> Result<PathBuf, Box<dyn Error>> {
    let config_path = root.join("deixis.toml");
    let args = case
        .args
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let initialization_options = if case.initialization_options.is_empty() {
        String::new()
    } else {
        format!(
            "\n[servers.compatibility.initialization_options]\n{}\n",
            case.initialization_options
        )
    };

    fs::write(
        &config_path,
        format!(
            r#"[servers.compatibility]
command = {}
args = [{}]
file_extensions = {{ {} = {} }}

[servers.compatibility.timeouts]
startup_ms = {STARTUP_TIMEOUT_MS}
request_ms = {REQUEST_TIMEOUT_MS}
shutdown_ms = {SHUTDOWN_TIMEOUT_MS}
{initialization_options}"#,
            serde_json::to_string(&command.to_string_lossy())?,
            args,
            serde_json::to_string(case.extension)?,
            serde_json::to_string(case.language_id)?,
        ),
    )?;
    Ok(config_path)
}

fn unique_dir(name: &str) -> Result<PathBuf, io::Error> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let path = env::temp_dir().join(format!(
        "deixis-compatibility-{name}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

#[test]
fn locates_the_last_hover_target_in_utf8_coordinates()
-> Result<(), Box<dyn Error>> {
    let source = "const café = 1;\nconst value = café;\n";
    assert_eq!(last_position(source, "café")?, Position::new(1, 14));
    Ok(())
}
