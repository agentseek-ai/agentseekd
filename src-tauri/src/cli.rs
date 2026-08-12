// CLI runtime detection: version comparison, dependency checking,
// agentseek/uv program resolution, and CLI process execution.

fn runtime_path() -> std::ffi::OsString {
    let mut paths = Vec::new();
    if let Some(runtime_root) = env::var_os("AGENTSEEK_DESKTOP_RUNTIME_DIR") {
        let versions_dir = PathBuf::from(runtime_root).join("nvm/versions/node");
        let mut managed_bins = fs::read_dir(versions_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.join("bin/node").is_file())
            .collect::<Vec<_>>();
        managed_bins.sort_by_key(|path| {
            path.file_name()
                .map(|name| numeric_version(&name.to_string_lossy()))
                .unwrap_or_default()
        });
        paths.extend(managed_bins.into_iter().rev().map(|path| path.join("bin")));
    }
    if let Some(managed_node_bin) = env::var_os("AGENTSEEK_DESKTOP_NODE_BIN") {
        paths.push(PathBuf::from(managed_node_bin));
    }
    if let Some(home) = env::var_os("HOME") {
        paths.push(PathBuf::from(&home).join(".local/bin"));
        paths.push(PathBuf::from(&home).join(".cargo/bin"));
        paths.push(PathBuf::from(&home).join(".pyenv/shims"));
        paths.push(PathBuf::from(home).join(".pyenv/bin"));
    }
    paths.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/Library/Frameworks/Python.framework/Versions/3.9/bin"),
    ]);
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).unwrap_or_default()
}

fn managed_runtime_root() -> Option<PathBuf> {
    env::var_os("AGENTSEEK_DESKTOP_RUNTIME_DIR").map(PathBuf::from)
}

fn managed_node_bin(runtime_root: &Path, node_version: &str) -> PathBuf {
    if cfg!(windows) {
        let architecture = if cfg!(target_arch = "aarch64") {
            "win-arm64"
        } else {
            "win-x64"
        };
        runtime_root.join(format!("node-v{node_version}-{architecture}"))
    } else {
        let versions_dir = runtime_root.join("nvm").join("versions").join("node");
        let exact = versions_dir.join(format!("v{node_version}")).join("bin");
        if exact.join("node").is_file() {
            return exact;
        }
        let major = numeric_version(node_version).first().copied();
        let mut candidates = fs::read_dir(&versions_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.join("bin/node").is_file()
                    && path
                        .file_name()
                        .map(|name| {
                            numeric_version(&name.to_string_lossy()).first().copied() == major
                        })
                        .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|path| {
            path.file_name()
                .map(|name| numeric_version(&name.to_string_lossy()))
                .unwrap_or_default()
        });
        candidates
            .pop()
            .map(|path| path.join("bin"))
            .unwrap_or(exact)
    }
}

const PYTHON_CHILD_ENV_VARS: [&str; 6] = [
    "PYTHONHOME",
    "PYTHONPATH",
    "VIRTUAL_ENV",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "CONDA_PROMPT_MODIFIER",
];

const LINUX_PYTHON_CHILD_ENV_VARS: [&str; 4] = [
    "PYTHONPLATLIBDIR",
    "PYTHONEXECUTABLE",
    "PYTHONSTARTUP",
    "UV_PYTHON",
];

const APPIMAGE_CHILD_ENV_VARS: [&str; 16] = [
    "APPDIR",
    "APPIMAGE",
    "ARGV0",
    "LD_LIBRARY_PATH",
    "LD_PRELOAD",
    "GIO_EXTRA_MODULES",
    "GIO_MODULE_DIR",
    "GTK_PATH",
    "GDK_PIXBUF_MODULE_FILE",
    "GDK_PIXBUF_MODULEDIR",
    "GST_PLUGIN_PATH",
    "GST_PLUGIN_SYSTEM_PATH",
    "GST_PLUGIN_SCANNER",
    "QT_PLUGIN_PATH",
    "QML2_IMPORT_PATH",
    "XDG_DATA_DIRS",
];

fn remove_python_environment(command: &mut Command) {
    for variable in PYTHON_CHILD_ENV_VARS {
        command.env_remove(variable);
    }
}

#[cfg(any(target_os = "linux", test))]
fn remove_appimage_environment(command: &mut Command) {
    for variable in APPIMAGE_CHILD_ENV_VARS {
        command.env_remove(variable);
    }
}

fn configure_python_command(command: &mut Command) {
    remove_python_environment(command);
    #[cfg(target_os = "linux")]
    {
        for variable in LINUX_PYTHON_CHILD_ENV_VARS {
            command.env_remove(variable);
        }
        remove_appimage_environment(command);
    }
}

fn configured_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("PATH", runtime_path());
    // Prevent Python from importing a local agentseek source tree
    // that may shadow the installed package when CWD contains agentseek/.
    command.current_dir(std::env::temp_dir());
    // External runtimes must not inherit Python or AppImage paths from the
    // desktop process. Those paths can point at a transient AppImage mount.
    configure_python_command(&mut command);
    command
}

fn curl_program() -> &'static str {
    if cfg!(target_os = "macos") {
        "/usr/bin/curl"
    } else {
        "curl"
    }
}

fn command_version(program: &str, arg: &str) -> Option<String> {
    let output = configured_command(program).arg(arg).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    combined
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
}

fn version_at_least(value: &str, minimum: &[u64]) -> bool {
    let current = numeric_version(value);
    if current.is_empty() {
        return false;
    }
    for index in 0..minimum.len().max(current.len()) {
        let current_part = current.get(index).copied().unwrap_or(0);
        let minimum_part = minimum.get(index).copied().unwrap_or(0);
        if current_part != minimum_part {
            return current_part > minimum_part;
        }
    }
    true
}

fn meets_requirement(value: &str, minimum: &str) -> bool {
    version_at_least(value, &numeric_version(minimum))
}

fn platform_id() -> String {
    if cfg!(target_os = "macos") {
        return "macos".to_string();
    }
    if cfg!(target_os = "windows") {
        return "windows".to_string();
    }
    if cfg!(target_os = "linux") {
        let distribution = fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|content| {
                content.lines().find_map(|line| {
                    line.strip_prefix("ID=")
                        .map(|value| value.trim_matches(['\"', '\'']).to_lowercase())
                })
            })
            .unwrap_or_else(|| "linux".to_string());
        return match distribution.as_str() {
            "ubuntu" | "debian" | "linuxmint" | "pop" => "debian".to_string(),
            "centos" | "rhel" | "fedora" | "rocky" | "almalinux" | "ol" => "rhel".to_string(),
            _ => "linux".to_string(),
        };
    }
    "unknown".to_string()
}

fn dependency_commands(
    requirements: &RuntimeRequirements,
    platform: &str,
    managed_runtime_root: Option<&Path>,
    uv_available: bool,
    git_available: bool,
) -> HashMap<String, String> {
    let mut commands = HashMap::new();
    let node_version = &requirements.versions.node.managed;
    let node_major = numeric_version(node_version)
        .first()
        .copied()
        .unwrap_or_default();
    let nvm_version = &requirements.versions.nvm.managed;
    let nvm_installer = requirements
        .sources
        .nvm_installer_template
        .replace("{version}", nvm_version);
    commands.insert(
        "uv".to_string(),
        if uv_available {
            "uv self update".to_string()
        } else {
            format!("curl -LsSf {} | sh", requirements.sources.uv_installer)
        },
    );
    let managed_root = managed_runtime_root
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "<AgentSeek data>/runtime".to_string());
    let managed_nvm = PathBuf::from(&managed_root).join("nvm");
    let managed_node_command = match platform {
        "macos" | "debian" | "rhel" | "linux" => format!(
            "unset npm_config_prefix && export NVM_DIR=\"{}\" PROFILE=/dev/null NVM_METHOD=script GIT_TERMINAL_PROMPT=0 && curl -fL {} | bash && . \"{}/nvm.sh\" && nvm install {} && node --version && npm --version",
            managed_nvm.to_string_lossy(),
            nvm_installer,
            managed_nvm.to_string_lossy(),
            node_major,
        ),
        "windows" => format!(
            "Downloading Node.js {} official ZIP to {} (for AgentSeek Desktop only)",
            node_version, managed_root
        ),
        _ => format!(
            "Installing Node.js {} to AgentSeek Desktop private runtime directory {}",
            node_version, managed_root
        ),
    };
    let git = match platform {
        "macos" => {
            if git_available {
                "brew upgrade git"
            } else {
                "brew install git"
            }
        }
        "debian" => "sudo apt-get update && sudo apt-get install -y git",
        "rhel" => "sudo dnf install -y git",
        "windows" => "winget install --id Git.Git",
        _ => "Please install the required Git version using your system package manager",
    };
    commands.insert("node".to_string(), managed_node_command.clone());
    commands.insert("npm".to_string(), managed_node_command);
    commands.insert("git".to_string(), git.to_string());
    commands.insert(
        "agentseek".to_string(),
        "uv tool install --upgrade agentseek".to_string(),
    );
    commands
}

fn program_from_login_shell(program: &str) -> Option<String> {
    if program.is_empty()
        || !program.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return None;
    }
    #[cfg(windows)]
    let output = Command::new("where.exe").arg(program).output().ok()?;
    #[cfg(not(windows))]
    let output = {
        let shell = env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .unwrap_or_else(|| {
                if cfg!(target_os = "macos") {
                    PathBuf::from("/bin/zsh")
                } else {
                    PathBuf::from("/bin/bash")
                }
            });
        Command::new(shell)
            .args(["-lic", &format!("command -v {program}")])
            .stderr(Stdio::null())
            .output()
            .ok()?
    };
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| Path::new(line).is_file())
        .map(str::to_string)
}

fn resolved_program(program: &str, version_arg: &str) -> Option<String> {
    if command_version(program, version_arg).is_some() {
        return Some(program.to_string());
    }
    let resolved = program_from_login_shell(program)?;
    command_version(&resolved, version_arg).map(|_| resolved)
}

fn uv_program() -> Option<String> {
    if let Ok(program) = env::var("AGENTSEEK_DESKTOP_UV") {
        if command_version(&program, "--version").is_some() {
            return Some(program);
        }
    }
    let mut candidates = Vec::new();
    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".local/bin/uv"));
        candidates.push(PathBuf::from(&home).join(".cargo/bin/uv"));
        candidates.push(PathBuf::from(home).join(".pyenv/shims/uv"));
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/uv"),
        PathBuf::from("/usr/local/bin/uv"),
        PathBuf::from("/Library/Frameworks/Python.framework/Versions/3.9/bin/uv"),
    ]);
    let located = candidates
        .into_iter()
        .find(|candidate| command_version(&candidate.to_string_lossy(), "--version").is_some())
        .map(|candidate| candidate.to_string_lossy().to_string());
    if located.is_some() {
        return located;
    }
    resolved_program("uv", "--version")
}

fn agentseek_program() -> String {
    if let Ok(program) = env::var("AGENTSEEK_CLI") {
        return program;
    }
    if let Some(uv) = uv_program() {
        let output = configured_command(uv)
            .args(["tool", "dir", "--bin"])
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let bin_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !bin_dir.is_empty() {
                    let executable = if cfg!(windows) {
                        "agentseek.exe"
                    } else {
                        "agentseek"
                    };
                    let candidate = PathBuf::from(bin_dir).join(executable);
                    if candidate.is_file() {
                        return candidate.to_string_lossy().to_string();
                    }
                }
            }
        }
    }
    if let Some(program) = resolved_program("agentseek", "--help") {
        return program;
    }
    "agentseek".to_string()
}

fn parse_uv_tool_version(content: &str, tool_name: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let name = parts.next()?;
        let version = parts.next()?;
        if name == tool_name && version.starts_with('v') {
            Some(format!("{tool_name} {}", version.trim_start_matches('v')))
        } else {
            None
        }
    })
}

fn parse_agentseek_version(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let trimmed = line.trim();
        let normalized = trimmed.to_ascii_lowercase();
        if normalized.starts_with("agentseek v") && !numeric_version(trimmed).is_empty() {
            Some(trimmed.to_string())
        } else {
            None
        }
    })
}

fn agentseek_command_version(program: &str) -> Option<String> {
    let output = configured_command(program).arg("version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let content = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_agentseek_version(&content)
}

fn uv_tool_version(tool_name: &str) -> Option<String> {
    let uv = uv_program()?;
    let output = configured_command(uv)
        .args(["tool", "list"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let content = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_uv_tool_version(&content, tool_name)
}

fn parse_agentseek_package_version(content: &[u8]) -> Result<String, String> {
    let metadata: serde_json::Value = serde_json::from_slice(content)
        .map_err(|error| format!("AgentSeek package metadata format error: {error}"))?;
    let version = metadata
        .get("info")
        .and_then(|info| info.get("version"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if numeric_version(&version).is_empty() {
        Err("AgentSeek package metadata has no valid latest version".to_string())
    } else {
        Ok(version)
    }
}

fn latest_agentseek_version(requirements: &RuntimeRequirements) -> Result<String, String> {
    if let Ok(version) = env::var("AGENTSEEK_DESKTOP_AGENTSEEK_LATEST_VERSION") {
        if !numeric_version(&version).is_empty() {
            return Ok(version);
        }
    }
    let output = configured_command(curl_program())
        .args([
            "-fsSL",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            "--retry",
            "2",
            &requirements.sources.agentseek_package_metadata,
        ])
        .output()
        .map_err(|error| format!("Failed to query AgentSeek latest version: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by system".to_string());
        return Err(if stderr.is_empty() {
            format!("Failed to query AgentSeek latest version (curl exit status: {status})")
        } else {
            format!("Failed to query AgentSeek latest version (curl exit status: {status}): {stderr}")
        });
    }
    parse_agentseek_package_version(&output.stdout)
}

fn cli_parts() -> (String, Vec<String>) {
    (agentseek_program(), Vec::new())
}

fn agentseek_update_available(installed: &str, latest: Option<&str>, available: bool) -> bool {
    available && latest.is_some_and(|latest_version| !meets_requirement(installed, latest_version))
}

fn current_cli_status(check_latest: bool) -> Result<CliStatus, String> {
    let requirements = load_runtime_requirements(DEFAULT_RUNTIME_REQUIREMENTS)?;
    let uv = uv_program();
    let uv_version = uv
        .as_deref()
        .and_then(|program| command_version(program, "--version"))
        .unwrap_or_default();
    let uv_path = uv.clone().unwrap_or_default();
    let program = agentseek_program();
    let cli_version = agentseek_command_version(&program)
        .or_else(|| command_version(&program, "--version"))
        .or_else(|| uv_tool_version("agentseek"))
        .unwrap_or_default();
    let cli_available = command_version(&program, "--help").is_some();
    let node_version = resolved_program("node", "--version")
        .and_then(|program| command_version(&program, "--version"))
        .unwrap_or_default();
    let npm_version = resolved_program("npm", "--version")
        .and_then(|program| command_version(&program, "--version"))
        .unwrap_or_default();
    let git_version = resolved_program("git", "--version")
        .and_then(|program| command_version(&program, "--version"))
        .unwrap_or_default();
    let uv_compatible = meets_requirement(&uv_version, &requirements.versions.uv.minimum);
    let node_compatible = meets_requirement(&node_version, &requirements.versions.node.minimum);
    let npm_compatible = meets_requirement(&npm_version, &requirements.versions.npm.minimum);
    let git_compatible = meets_requirement(&git_version, &requirements.versions.git.minimum);
    let cli_latest_version = check_latest
        .then(|| latest_agentseek_version(&requirements).ok())
        .flatten();
    let cli_latest_version_checked = cli_latest_version.is_some();
    let cli_compatible = meets_requirement(&cli_version, &requirements.versions.agentseek.minimum);
    let cli_update_available =
        agentseek_update_available(&cli_version, cli_latest_version.as_deref(), cli_available);
    let platform = platform_id();
    let runtime_root = managed_runtime_root();
    let dependency_commands = dependency_commands(
        &requirements,
        &platform,
        runtime_root.as_deref(),
        !uv_version.is_empty(),
        !git_version.is_empty(),
    );
    let node_managed = runtime_root
        .as_deref()
        .map(|root| managed_node_bin(root, &requirements.versions.node.managed))
        .map(|bin| {
            bin.join(if cfg!(windows) { "node.exe" } else { "node" })
                .is_file()
        })
        .unwrap_or(false);
    let prerequisites_ready = uv_compatible && node_compatible && npm_compatible && cli_compatible;
    let minimum_versions = [
        ("uv", requirements.versions.uv.minimum.as_str()),
        ("node", requirements.versions.node.minimum.as_str()),
        ("npm", requirements.versions.npm.minimum.as_str()),
        ("git", requirements.versions.git.minimum.as_str()),
        (
            "agentseek",
            requirements.versions.agentseek.minimum.as_str(),
        ),
    ]
    .into_iter()
    .map(|(name, version)| (name.to_string(), version.to_string()))
    .collect();
    Ok(CliStatus {
        platform,
        dependency_commands,
        minimum_versions,
        node_managed,
        uv_available: !uv_version.is_empty(),
        uv_path,
        cli_available,
        cli_compatible,
        cli_update_available,
        cli_latest_version: cli_latest_version.unwrap_or_default(),
        cli_latest_version_checked,
        uv_version,
        cli_version,
        node_available: !node_version.is_empty(),
        node_compatible,
        node_version,
        npm_available: !npm_version.is_empty(),
        npm_compatible,
        npm_version,
        git_available: !git_version.is_empty(),
        git_compatible,
        git_version,
        uv_compatible,
        prerequisites_ready,
        install_command: "uv tool install agentseek".to_string(),
    })
}

/// Execute the CLI with optional stdin input (for interactive cookiecutter prompts).
/// Keeps stdin handle alive until the process exits to avoid EOF-induced infinite loops.
fn run_cli_with_input(
    args: &[&str],
    cwd: Option<&Path>,
    answers: Option<&str>,
) -> Result<CommandResult, String> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let (program, prefix) = cli_parts();
    let mut command = configured_command(&program);
    command.args(&prefix).args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let printable = std::iter::once(program.as_str())
        .chain(prefix.iter().map(String::as_str))
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");

    let mut child = command
        .spawn()
        .map_err(|error| format!("Unable to execute {printable}: {error}"))?;

    // Write answers to stdin (if provided) and keep the handle alive until
    // the child exits. Dropping stdin early would send EOF, causing
    // cookiecutter's `while True` prompt loop to spin forever.
    let mut stdin_handle = child
        .stdin
        .take()
        .ok_or_else(|| "Failed to open stdin for child process".to_string())?;
    if let Some(answers) = answers {
        if let Err(error) = stdin_handle.write_all(answers.as_bytes()) {
            let _ = stdin_handle.flush();
            // The child cannot receive answers; fail fast instead of hanging.
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Failed to write answers to CLI stdin: {error}"));
        }
        if let Err(error) = stdin_handle.flush() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Failed to flush CLI stdin: {error}"));
        }
    }

    // Spawn threads to read stdout/stderr (prevents pipe buffer deadlock);
    // results are delivered via channels so we can bound the join wait.
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to open stdout for child process".to_string())?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to open stderr for child process".to_string())?;
    let (stdout_tx, stdout_rx) = mpsc::channel::<String>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<String>();
    let stdout_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout_pipe.read_to_string(&mut s);
        let _ = stdout_tx.send(s);
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        let _ = stderr_tx.send(s);
    });

    // Poll with timeout (10 minutes) to prevent infinite hang on EOF deadlock.
    let deadline = Instant::now() + Duration::from_secs(600);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // Reap the child to avoid zombies.
                    return Err(format!("Command timed out after 600 seconds: {printable}"));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Failed to wait for child process: {e}"));
            }
        }
    };

    // Child exited; safe to drop stdin now.
    drop(stdin_handle);

    // Bound the pipe-drain wait: a grandchild holding the pipe write end could
    // otherwise keep the reader threads alive forever.
    let stdout = stdout_rx.recv_timeout(Duration::from_secs(30)).unwrap_or_default();
    let stderr = stderr_rx.recv_timeout(Duration::from_secs(30)).unwrap_or_default();
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    let combined = format!("{}{}", stdout, stderr);

    Ok(CommandResult {
        code: status.code().unwrap_or(1),
        output: combined.trim().to_string(),
        command: printable,
    })
}

// ---------------------------------------------------------------------------
// CLI status & system info commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn cli_status(check_latest: Option<bool>) -> Result<CliStatus, String> {
    tauri::async_runtime::spawn_blocking(move || current_cli_status(check_latest.unwrap_or(true)))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
fn system_info(state: State<'_, DesktopState>) -> SystemInfo {
    let (program, prefix) = cli_parts();
    let config = state
        .storage_config
        .lock()
        .ok()
        .map(|config| config.clone())
        .unwrap_or_default();
    let effective_mode = state
        .effective_storage_mode
        .lock()
        .map(|mode| mode.clone())
        .unwrap_or_else(|_| "sqlite_embedded".to_string());
    let (data_path, storage) = match effective_mode.as_str() {
        "seekdb_embedded" => (config.path, "Embedded SeekDB".to_string()),
        "seekdb_server" | "oceanbase_server" => (
            format!("{}:{} / {}", config.host, config.port, config.database),
            "SeekDB / OceanBase Server".to_string(),
        ),
        _ => (
            sqlite_database_path(&state.data_dir, &config)
                .to_string_lossy()
                .to_string(),
            "Embedded SQLite".to_string(),
        ),
    };
    let docker_status = check_docker();
    SystemInfo {
        app_name: "AgentSeek Desktop".to_string(),
        version: env!("APP_VERSION").to_string(),
        data_path,
        cli_strategy: std::iter::once(program)
            .chain(prefix)
            .collect::<Vec<_>>()
            .join(" "),
        storage: format!("{storage} (desktop state only; isolated from template instances)"),
        docker_available: docker_status.cli_available,
        docker_compose_available: docker_status.compose_v2_available,
        docker_running: docker_status.daemon_running,
    }
}

#[cfg(test)]
mod tests_cli {
    use super::*;

    fn removed_environment(command: &Command) -> HashSet<String> {
        command
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then(|| name.to_string_lossy().to_string()))
            .collect()
    }

    #[test]
    fn configured_commands_remove_python_runtime_environment() {
        let command = configured_command("agentseek");
        let removed = removed_environment(&command);

        for variable in PYTHON_CHILD_ENV_VARS {
            assert!(removed.contains(variable), "{variable} should be removed");
        }
    }

    #[test]
    fn dependency_versions_are_compared_across_command_formats() {
        assert!(version_at_least("uv 0.7.11", &[0, 7, 0]));
        assert!(version_at_least("v20.19.0", &[20, 19, 0]));
        assert!(version_at_least("git version 2.30.0", &[2, 30, 0]));
        assert!(!version_at_least("9.9.0", &[10, 0, 0]));
        assert!(!version_at_least("not installed", &[1, 0, 0]));
    }
    #[test]
    fn only_secret_environment_keys_are_redacted() {
        assert!(is_secret_env_key("OPENAI_API_KEY"));
        assert!(is_secret_env_key("DATABASE_PASSWORD"));
        assert!(!is_secret_env_key("FRONTEND_PORT"));
        assert!(!is_secret_env_key("COPILOTKIT_PORT"));
    }
    #[test]
    fn agentseek_version_is_read_from_uv_tool_list() {
        let output = "agentseek v0.0.4\n- agentseek\n";
        assert_eq!(
            parse_uv_tool_version(output, "agentseek").as_deref(),
            Some("agentseek 0.0.4")
        );
    }
    #[test]
    fn agentseek_version_is_read_after_banner() {
        let output = "    _                    _\n   / \\   __ _  ___ _ __\nAGENTSEEK v0.0.4\n";
        assert_eq!(
            parse_agentseek_version(output).as_deref(),
            Some("AGENTSEEK v0.0.4")
        );
    }
    #[test]
    fn agentseek_latest_version_is_read_from_package_metadata() {
        assert_eq!(
            parse_agentseek_package_version(br#"{"info":{"version":"0.0.5"}}"#)
                .expect("parse package version"),
            "0.0.5"
        );
    }
    #[test]
    fn numeric_version_empty_string_returns_empty_vec() {
        assert!(numeric_version("").is_empty());
    }
    #[test]
    fn numeric_version_non_numeric_returns_empty_vec() {
        assert!(numeric_version("abc").is_empty());
        assert!(numeric_version("no version here").is_empty());
    }
    #[test]
    fn numeric_version_pre_release_suffix_stops_at_suffix() {
        let v = numeric_version("1.2.3-alpha");
        assert_eq!(v, vec![1, 2, 3]);
    }
    #[test]
    fn version_at_least_empty_version_returns_false() {
        assert!(!version_at_least("", &[1, 0]));
    }
    #[test]
    fn meets_requirement_empty_version_returns_false() {
        assert!(!meets_requirement("", "1.0.0"));
    }
    #[test]
    fn meets_requirement_non_numeric_returns_false() {
        assert!(!meets_requirement("abc", "1.0.0"));
    }
    #[test]
    fn meets_requirement_pre_release_version() {
        // "1.2.3-alpha" should parse to [1, 2, 3] and meet "1.2.0"
        assert!(meets_requirement("1.2.3-alpha", "1.2.0"));
        // But should NOT meet "1.3.0"
        assert!(!meets_requirement("1.2.3-alpha", "1.3.0"));
    }
}
