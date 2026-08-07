// Runtime installation: shell/PowerShell install scripts, install plan
// preparation, and terminal launching.

fn run_dependency_command(program: &str, args: &[&str], printable: &str) -> Result<String, String> {
    let output = configured_command(program)
        .args(args)
        .output()
        .map_err(|error| format!("Failed to execute {printable}: {error}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_string();
    if output.status.success() {
        Ok(combined)
    } else if combined.is_empty() {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by system".to_string());
        Err(format!("Command execution failed: {printable} (exit status: {status})"))
    } else {
        Err(combined)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn required_runtime_dependencies(status: &CliStatus) -> Vec<String> {
    let mut dependencies = Vec::new();
    if !status.uv_compatible {
        dependencies.push("uv".to_string());
    }
    if !status.node_compatible || !status.npm_compatible {
        dependencies.push("node/npm".to_string());
    }
    if !status.cli_compatible || status.cli_update_available {
        dependencies.push("agentseek".to_string());
    }
    dependencies
}

fn windows_uv_installer(requirements: &RuntimeRequirements) -> String {
    requirements
        .sources
        .uv_installer_windows
        .clone()
        .unwrap_or_else(|| {
            requirements
                .sources
                .uv_installer
                .strip_suffix(".sh")
                .map(|value| format!("{value}.ps1"))
                .unwrap_or_else(|| requirements.sources.uv_installer.clone())
        })
}

fn posix_runtime_install_script(
    requirements: &RuntimeRequirements,
    status: &CliStatus,
    task_dir: &Path,
    runtime_root: &Path,
) -> String {
    let status_file = task_dir.join("status.json");
    let log_file = task_dir.join("install.log");
    let nvm_dir = runtime_root.join("nvm");
    let nvm_installer = requirements
        .sources
        .nvm_installer_template
        .replace("{version}", &requirements.versions.nvm.managed);
    let node_major = numeric_version(&requirements.versions.node.managed)
        .first()
        .copied()
        .unwrap_or_default();
    let mut lines = vec![
        "#!/usr/bin/env bash".to_string(),
        "set -eo pipefail".to_string(),
        format!("STATUS_FILE={}", shell_quote(&status_file.to_string_lossy())),
        format!("LOG_FILE={}", shell_quote(&log_file.to_string_lossy())),
        format!("TASK_DIR={}", shell_quote(&task_dir.to_string_lossy())),
        "mkdir -p \"$(dirname \"$STATUS_FILE\")\"".to_string(),
        ": > \"$LOG_FILE\"".to_string(),
        "STAGE=starting".to_string(),
        "printf '%s\\n' '{\"status\":\"running\",\"stage\":\"starting\"}' > \"$STATUS_FILE\"".to_string(),
        "exec > >(tee -a \"$LOG_FILE\") 2>&1".to_string(),
        "on_exit() {".to_string(),
        "  code=$?".to_string(),
        "  if [ \"$code\" -ne 0 ]; then".to_string(),
        "    printf '{\"status\":\"failed\",\"stage\":\"%s\",\"code\":%s}\\n' \"$STAGE\" \"$code\" > \"$STATUS_FILE\"".to_string(),
        "    printf '\\nInstallation failed. Press Enter to close this terminal.\\n'".to_string(),
        "    read -r _ || true".to_string(),
        "  fi".to_string(),
        "}".to_string(),
        "trap on_exit EXIT".to_string(),
        "export PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH\"".to_string(),
        "run_shell_installer() {".to_string(),
        "  url=$1".to_string(),
        "  label=$2".to_string(),
        "  installer_file=\"$TASK_DIR/${label}-installer.sh\"".to_string(),
        "  attempt=1".to_string(),
        "  while [ \"$attempt\" -le 3 ]; do".to_string(),
        "    echo \"Installing $label (attempt $attempt/3)\"".to_string(),
        "    rm -f \"$installer_file\" \"$installer_file.tmp\"".to_string(),
        "    if command -v curl >/dev/null 2>&1; then".to_string(),
        "      curl -fL --silent --show-error --connect-timeout 10 --max-time 60 --retry 1 --retry-all-errors --retry-delay 2 --output \"$installer_file.tmp\" \"$url\" || true".to_string(),
        "    elif command -v wget >/dev/null 2>&1; then".to_string(),
        "      wget -qO \"$installer_file.tmp\" --timeout=30 --tries=3 --waitretry=2 \"$url\" || true".to_string(),
        "    else".to_string(),
        "      echo \"Neither curl nor wget is available; cannot install $label.\" >&2; return 1".to_string(),
        "    fi".to_string(),
        "    if [ -s \"$installer_file.tmp\" ] && [ \"$(wc -c < \"$installer_file.tmp\")\" -ge 1024 ] && bash -n \"$installer_file.tmp\"; then".to_string(),
        "      mv \"$installer_file.tmp\" \"$installer_file\"".to_string(),
        "      if bash \"$installer_file\"; then return 0; fi".to_string(),
        "    else".to_string(),
        "      echo \"Downloaded $label installer is incomplete or invalid; it will not be executed.\" >&2".to_string(),
        "    fi".to_string(),
        "    rm -f \"$installer_file\" \"$installer_file.tmp\"".to_string(),
        "    if [ \"$attempt\" -lt 3 ]; then echo \"$label download failed. Retrying in 3 seconds.\"; sleep 3; fi".to_string(),
        "    attempt=$((attempt + 1))".to_string(),
        "  done".to_string(),
        "  return 1".to_string(),
        "}".to_string(),
        "echo 'AgentSeek Desktop runtime installation'".to_string(),
        "echo '================================='".to_string(),
    ];

    if !status.uv_compatible {
        lines.extend([
            "STAGE=uv".to_string(),
            "printf '%s\\n' '{\"status\":\"running\",\"stage\":\"uv\"}' > \"$STATUS_FILE\""
                .to_string(),
            "echo '[1/3] Install or upgrade uv'".to_string(),
        ]);
        lines.push(format!(
            "run_shell_installer {} uv",
            shell_quote(&requirements.sources.uv_installer)
        ));
        lines.push("UV_BIN=\"$HOME/.local/bin/uv\"".to_string());
    } else {
        lines.push(format!("UV_BIN={}", shell_quote(&status.uv_path)));
        lines.push("echo '[1/3] uv already meets the requirement; skipping'".to_string());
    }

    if !status.node_compatible || !status.npm_compatible {
        lines.extend([
            "STAGE=node".to_string(),
            "printf '%s\\n' '{\"status\":\"running\",\"stage\":\"node\"}' > \"$STATUS_FILE\""
                .to_string(),
            "echo '[2/3] Install AgentSeek Desktop private Node.js / npm'".to_string(),
            "unset npm_config_prefix".to_string(),
            format!("export NVM_DIR={}", shell_quote(&nvm_dir.to_string_lossy())),
            "export PROFILE=/dev/null".to_string(),
            "mkdir -p \"$NVM_DIR\"".to_string(),
            // Use nvm's script installer so it never clones a repository or
            // prompts the user for Git credentials in a desktop install.
            "export NVM_METHOD=script".to_string(),
            "export GIT_TERMINAL_PROMPT=0".to_string(),
            format!(
                "if [ ! -s \"$NVM_DIR/nvm.sh\" ]; then run_shell_installer {installer} nvm; fi",
                installer = shell_quote(&nvm_installer),
            ),
            ". \"$NVM_DIR/nvm.sh\"".to_string(),
            format!("if curl -fsI --connect-timeout 2 --max-time 3 \"{mirror}/v24.18.0/SHASUMS256.txt\" > /dev/null 2>&1; then export NVM_NODEJS_ORG_MIRROR={mirror}; fi", mirror = NVM_NODEJS_MIRROR),
            format!("nvm install {node_major}"),
            "node --version".to_string(),
            "npm --version".to_string(),
        ]);
    } else {
        lines
            .push("echo '[2/3] Node.js / npm already meet the requirements; skipping'".to_string());
    }

    if !status.cli_compatible || status.cli_update_available {
        lines.extend([
            "STAGE=agentseek".to_string(),
            "printf '%s\\n' '{\"status\":\"running\",\"stage\":\"agentseek\"}' > \"$STATUS_FILE\""
                .to_string(),
            "echo '[3/3] Install or upgrade AgentSeek CLI'".to_string(),
            "if [ ! -x \"${UV_BIN:-}\" ]; then UV_BIN=\"$(command -v uv)\"; fi".to_string(),
            "\"$UV_BIN\" tool install --upgrade agentseek".to_string(),
            "\"$UV_BIN\" tool update-shell".to_string(),
            "AGENTSEEK_BIN=\"$HOME/.local/bin/agentseek\"".to_string(),
            "if [ ! -x \"$AGENTSEEK_BIN\" ]; then AGENTSEEK_BIN=\"$(command -v agentseek)\"; fi"
                .to_string(),
            "\"$AGENTSEEK_BIN\" version".to_string(),
        ]);
    } else {
        lines
            .push("echo '[3/3] AgentSeek CLI already meets the requirement; skipping'".to_string());
    }

    lines.extend([
        "STAGE=complete".to_string(),
        "printf '%s\\n' '{\"status\":\"success\",\"stage\":\"complete\",\"code\":0}' > \"$STATUS_FILE\"".to_string(),
        "trap - EXIT".to_string(),
        "kill 0".to_string(),
        String::new(),
    ]);
    lines.join("\n")
}

fn windows_runtime_install_script(
    requirements: &RuntimeRequirements,
    status: &CliStatus,
    task_dir: &Path,
    runtime_root: &Path,
) -> String {
    let status_file = task_dir.join("status.json");
    let log_file = task_dir.join("install.log");
    let architecture = if cfg!(target_arch = "aarch64") {
        "win-arm64"
    } else {
        "win-x64"
    };
    let node_version = &requirements.versions.node.managed;
    let archive_name = format!("node-v{node_version}-{architecture}.zip");
    let node_root = runtime_root.join(format!("node-v{node_version}-{architecture}"));
    let node_url = format!(
        "{}/v{node_version}/{archive_name}",
        requirements.sources.node_distribution.trim_end_matches('/')
    );
    let checksum_url = format!(
        "{}/v{node_version}/SHASUMS256.txt",
        requirements.sources.node_distribution.trim_end_matches('/')
    );
    let uv_installer = windows_uv_installer(requirements);
    let mut lines = vec![
        "$ErrorActionPreference = 'Stop'".to_string(),
        format!(
            "$StatusFile = {}",
            powershell_quote(&status_file.to_string_lossy())
        ),
        format!(
            "$LogFile = {}",
            powershell_quote(&log_file.to_string_lossy())
        ),
        format!(
            "$TaskDir = {}",
            powershell_quote(&task_dir.to_string_lossy())
        ),
        "New-Item -ItemType Directory -Force -Path (Split-Path $StatusFile) | Out-Null".to_string(),
        "$Stage = 'starting'".to_string(),
        "'{\"status\":\"running\",\"stage\":\"starting\"}' | Set-Content -Encoding UTF8 $StatusFile".to_string(),
        "Start-Transcript -Path $LogFile -Force".to_string(),
        "function Invoke-DownloadWithRetry {".to_string(),
        "  param([string]$Uri, [string]$OutFile, [string]$Label)".to_string(),
        "  for ($Attempt = 1; $Attempt -le 6; $Attempt++) {".to_string(),
        "    try {".to_string(),
        "      Write-Host \"Downloading $Label (attempt $Attempt/6)\"".to_string(),
        "      Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $OutFile".to_string(),
        "      return".to_string(),
        "    } catch {".to_string(),
        "      if ($Attempt -eq 6) { throw }".to_string(),
        "      Write-Host 'Download failed. If security software prompts, allow the download. Retrying in 10 seconds.'".to_string(),
        "      Start-Sleep -Seconds 10".to_string(),
        "    }".to_string(),
        "  }".to_string(),
        "}".to_string(),
        "try {".to_string(),
        "  Write-Host 'AgentSeek Desktop runtime installation'".to_string(),
    ];
    if !status.uv_compatible {
        lines.push("  $Stage = 'uv'; ('{\"status\":\"running\",\"stage\":\"uv\"}') | Set-Content -Encoding UTF8 $StatusFile".to_string());
        if status.uv_available && !status.uv_path.is_empty() {
            lines.push("  Write-Host 'An outdated uv was found. The required version will be installed in the current user tool directory without changing the system installation.'".to_string());
        } else {
            lines.push("  Write-Host 'uv was not found; starting installation.'".to_string());
        }
        lines.push("  $UvInstaller = Join-Path $TaskDir 'uv-install.ps1'".to_string());
        lines.push(format!(
            "  Invoke-DownloadWithRetry -Uri {} -OutFile $UvInstaller -Label 'official uv installer'",
            powershell_quote(&uv_installer)
        ));
        lines.push("  & $UvInstaller".to_string());
        lines.push("  $UvBin = Join-Path $HOME '.local\\bin\\uv.exe'".to_string());
    } else {
        lines.push(format!("  $UvBin = {}", powershell_quote(&status.uv_path)));
    }
    if !status.node_compatible || !status.npm_compatible {
        lines.extend([
            "  $Stage = 'node'; ('{\"status\":\"running\",\"stage\":\"node\"}') | Set-Content -Encoding UTF8 $StatusFile".to_string(),
            format!("  $ArchiveName = {}", powershell_quote(&archive_name)),
            "  $Archive = Join-Path $env:TEMP $ArchiveName".to_string(),
            "  $Checksums = Join-Path $env:TEMP 'agentseek-node-SHASUMS256.txt'".to_string(),
            "  $ExtractDir = Join-Path $env:TEMP ('agentseek-node-' + [guid]::NewGuid())".to_string(),
            format!("  Invoke-DownloadWithRetry -Uri {} -OutFile $Archive -Label 'Node.js ZIP'", powershell_quote(&node_url)),
            format!("  Invoke-DownloadWithRetry -Uri {} -OutFile $Checksums -Label 'Node.js SHA-256 manifest'", powershell_quote(&checksum_url)),
            "  $Line = Get-Content $Checksums | Where-Object { $_ -match ('\\s+' + [regex]::Escape($ArchiveName) + '$') } | Select-Object -First 1".to_string(),
            "  if (-not $Line) { throw 'Node.js SHA-256 entry was not found' }".to_string(),
            "  $Expected = ($Line.Trim() -split '\\s+')[0].ToLowerInvariant()".to_string(),
            "  $Actual = (Get-FileHash -Algorithm SHA256 $Archive).Hash.ToLowerInvariant()".to_string(),
            "  if ($Expected -ne $Actual) { throw 'Node.js SHA-256 verification failed' }".to_string(),
            "  Expand-Archive -Path $Archive -DestinationPath $ExtractDir -Force".to_string(),
            format!("  $NodeRoot = {}", powershell_quote(&node_root.to_string_lossy())),
            "  if (Test-Path $NodeRoot) { Remove-Item -Recurse -Force $NodeRoot }".to_string(),
            "  New-Item -ItemType Directory -Force -Path (Split-Path $NodeRoot) | Out-Null".to_string(),
            "  Move-Item (Join-Path $ExtractDir ($ArchiveName -replace '\\.zip$','')) $NodeRoot".to_string(),
            format!("  & (Join-Path $NodeRoot 'npm.cmd') install -g npm@{}", requirements.versions.npm.managed),
            "  if ($LASTEXITCODE -ne 0) { throw \"npm installation failed with exit code $LASTEXITCODE\" }".to_string(),
            "  Remove-Item -Force $Archive,$Checksums".to_string(),
            "  Remove-Item -Recurse -Force $ExtractDir".to_string(),
        ]);
    }
    if !status.cli_compatible || status.cli_update_available {
        lines.extend([
            "  $Stage = 'agentseek'; ('{\"status\":\"running\",\"stage\":\"agentseek\"}') | Set-Content -Encoding UTF8 $StatusFile".to_string(),
            "  if (-not (Test-Path $UvBin)) { $UvBin = (Get-Command uv).Source }".to_string(),
            "  & $UvBin tool install --upgrade agentseek".to_string(),
            "  if ($LASTEXITCODE -ne 0) { throw \"AgentSeek CLI installation failed with exit code $LASTEXITCODE\" }".to_string(),
            "  & $UvBin tool update-shell".to_string(),
            "  if ($LASTEXITCODE -ne 0) { throw \"uv tool update-shell failed with exit code $LASTEXITCODE\" }".to_string(),
        ]);
    }
    lines.extend([
        "  $Stage = 'complete'; '{\"status\":\"success\",\"stage\":\"complete\",\"code\":0}' | Set-Content -Encoding UTF8 $StatusFile".to_string(),
        "} catch {".to_string(),
        "  $Message = $_.Exception.Message.Replace('\\','\\\\').Replace('\"','\\\"')".to_string(),
        "  ('{\"status\":\"failed\",\"stage\":\"' + $Stage + '\",\"code\":1,\"message\":\"' + $Message + '\"}') | Set-Content -Encoding UTF8 $StatusFile".to_string(),
        "  Write-Error $_".to_string(),
        "} finally {".to_string(),
        "  Stop-Transcript".to_string(),
        "}".to_string(),
        String::new(),
    ]);
    lines.join("\r\n")
}

fn prepare_runtime_install_plan(
    state: &DesktopState,
    force_agentseek_upgrade: bool,
) -> Result<RuntimeInstallPlan, String> {
    let requirements = load_runtime_requirements(DEFAULT_RUNTIME_REQUIREMENTS)?;
    let status = current_cli_status(true)?;
    if force_agentseek_upgrade
        && (!status.cli_update_available || status.cli_latest_version.is_empty())
    {
        return Err("AgentSeek CLI is already up to date".to_string());
    }
    let upgrade_target = if status.cli_update_available {
        Some(status.cli_latest_version.clone())
    } else {
        None
    };
    let dependencies = required_runtime_dependencies(&status);
    if dependencies.is_empty() {
        return Err("Current runtime environment already meets requirements".to_string());
    }
    let runtime_root = managed_runtime_root()
        .ok_or_else(|| "AgentSeek Desktop private runtime directory not yet initialized".to_string())?;
    let task_id = unique_stamp().to_string();
    let task_dir = state.data_dir.join("runtime-install").join(&task_id);
    fs::create_dir_all(&task_dir).map_err(|error| format!("Failed to create install task directory: {error}"))?;
    let (script_name, script) = if cfg!(windows) {
        (
            "install.ps1",
            windows_runtime_install_script(&requirements, &status, &task_dir, &runtime_root),
        )
    } else {
        (
            "install.command",
            posix_runtime_install_script(&requirements, &status, &task_dir, &runtime_root),
        )
    };
    let script_path = task_dir.join(script_name);
    #[cfg(unix)]
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o700)
            .open(&script_path)
            .map_err(|error| format!("Failed to create install script: {error}"))?;
        file.write_all(script.as_bytes())
            .map_err(|error| format!("Failed to write install script: {error}"))?;
    }
    #[cfg(windows)]
    fs::write(&script_path, &script).map_err(|error| format!("Failed to write install script: {error}"))?;
    fs::write(task_dir.join("status.json"), "{\"status\":\"pending\"}\n")
        .map_err(|error| format!("Failed to initialize install task state: {error}"))?;
    if let Some(target) = upgrade_target {
        fs::write(task_dir.join("agentseek-upgrade-target"), target)
            .map_err(|error| format!("Failed to record AgentSeek CLI upgrade target: {error}"))?;
    }
    Ok(RuntimeInstallPlan {
        task_id,
        script,
        script_path: script_path.to_string_lossy().to_string(),
        install_dir: runtime_root.to_string_lossy().to_string(),
        dependencies,
    })
}

fn launch_runtime_install_terminal(script_path: &Path) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        let output = Command::new("/usr/bin/open")
            .args(["-a", "Terminal"])
            .arg(script_path)
            .output()
            .map_err(|error| format!("Failed to open system terminal: {error}"))?;
        return if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        };
    }
    if cfg!(windows) {
        configured_command("cmd")
            .args(["/C", "start", "", "powershell.exe", "-NoProfile", "-File"])
            .arg(script_path)
            .spawn()
            .map_err(|error| format!("Failed to open PowerShell: {error}"))?;
        return Ok(());
    }
    let script = script_path.to_string_lossy().to_string();
    let candidates: [(&str, Vec<String>); 3] = [
        (
            "x-terminal-emulator",
            vec!["-e".to_string(), "bash".to_string(), script.clone()],
        ),
        (
            "gnome-terminal",
            vec!["--".to_string(), "bash".to_string(), script.clone()],
        ),
        (
            "konsole",
            vec!["-e".to_string(), "bash".to_string(), script],
        ),
    ];
    for (program, args) in candidates {
        if configured_command(program).args(args).spawn().is_ok() {
            return Ok(());
        }
    }
    Err("No available system terminal found; please install x-terminal-emulator, GNOME Terminal, or Konsole".to_string())
}

fn install_log_tail(path: &Path) -> String {
    let Ok(content) = fs::read(path) else {
        return String::new();
    };
    let start = content.len().saturating_sub(16 * 1024);
    String::from_utf8_lossy(&content[start..])
        .trim()
        .to_string()
}

fn runtime_install_task_dir(state: &DesktopState, task_id: &str) -> Result<PathBuf, String> {
    if task_id.is_empty() || !task_id.chars().all(|character| character.is_ascii_digit()) {
        return Err("Invalid install task ID".to_string());
    }
    Ok(state.data_dir.join("runtime-install").join(task_id))
}

// ---------------------------------------------------------------------------
// Runtime install commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn runtime_install_progress(
    task_id: String,
    state: State<'_, DesktopState>,
) -> Result<RuntimeInstallProgress, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let task_dir = runtime_install_task_dir(&state, &task_id)?;
        let status = fs::read_to_string(task_dir.join("status.json"))
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(content.trim()).ok())
            .unwrap_or_else(|| serde_json::json!({"status": "pending", "stage": "pending"}));
        Ok(RuntimeInstallProgress {
            status: status
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending")
                .to_string(),
            stage: status
                .get("stage")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending")
                .to_string(),
            log: install_log_tail(&task_dir.join("install.log")),
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn runtime_install_plan(
    force_agentseek_upgrade: Option<bool>,
    state: State<'_, DesktopState>,
) -> Result<RuntimeInstallPlan, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        prepare_runtime_install_plan(&state, force_agentseek_upgrade.unwrap_or(false))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn execute_runtime_install(
    task_id: String,
    state: State<'_, DesktopState>,
) -> Result<String, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let task_dir = runtime_install_task_dir(&state, &task_id)?;
        let script_path = task_dir.join(if cfg!(windows) {
            "install.ps1"
        } else {
            "install.command"
        });
        if !script_path.is_file() {
            return Err("Install script does not exist, please regenerate install plan".to_string());
        }
        state.log(
            None,
            "AgentSeek Desktop",
            "install",
            "info",
            format!(
                "Opened system terminal to execute runtime environment install script\n{}",
                script_path.display()
            ),
            Some(script_path.to_string_lossy().to_string()),
        );
        launch_runtime_install_terminal(&script_path)?;
        let status_path = task_dir.join("status.json");
        for _ in 0..3_600 {
            std::thread::sleep(Duration::from_millis(500));
            let Ok(content) = fs::read_to_string(&status_path) else {
                continue;
            };
            let Ok(status): Result<serde_json::Value, _> = serde_json::from_str(content.trim())
            else {
                continue;
            };
            match status.get("status").and_then(serde_json::Value::as_str) {
                Some("success") => {
                    let checked = current_cli_status(false)?;
                    if !checked.prerequisites_ready {
                        return Err(
                            "Install script completed, but some dependencies still do not meet version requirements; please re-check".to_string()
                        );
                    }
                    if let Ok(target) =
                        fs::read_to_string(task_dir.join("agentseek-upgrade-target"))
                    {
                        if !meets_requirement(&checked.cli_version, target.trim()) {
                            return Err(format!(
                                "AgentSeek CLI upgrade did not reach target version {}; currently detected {}",
                                target.trim(),
                                checked.cli_version
                            ));
                        }
                    }
                    state.log(
                        None,
                        "AgentSeek Desktop",
                        "install",
                        "success",
                        "Terminal install script completed; runtime environment check passed",
                        Some(script_path.to_string_lossy().to_string()),
                    );
                    return Ok(format!(
                        "Runtime environment installation completed\nLog: {}",
                        task_dir.join("install.log").display()
                    ));
                }
                Some("failed") => {
                    let tail = install_log_tail(&task_dir.join("install.log"));
                    state.log(
                        None,
                        "AgentSeek Desktop",
                        "install",
                        "error",
                        format!("Terminal install script execution failed\n{tail}"),
                        Some(script_path.to_string_lossy().to_string()),
                    );
                    return Err(if tail.is_empty() {
                        "Terminal install script execution failed; please check terminal output".to_string()
                    } else {
                        tail
                    });
                }
                _ => {}
            }
        }
        Err("Timed out waiting for terminal install result; please check terminal output and re-check".to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests_runtime_install {
    use super::*;

    #[test]
    fn bundled_runtime_requirements_are_valid() {
        let requirements: RuntimeRequirements =
            serde_json::from_str(DEFAULT_RUNTIME_REQUIREMENTS).expect("parse requirements");
        validate_runtime_requirements(&requirements).expect("validate requirements");
    }
    #[test]
    fn agentseek_updates_do_not_change_minimum_version_compatibility() {
        assert!(version_at_least("AGENTSEEK v0.0.4", &[0, 0, 4]));
        assert!(agentseek_update_available(
            "AGENTSEEK v0.0.4",
            Some("0.0.5"),
            true
        ));
        assert!(!agentseek_update_available(
            "AGENTSEEK v0.0.5",
            Some("0.0.5"),
            true
        ));
        assert!(!agentseek_update_available(
            "AGENTSEEK v0.0.6",
            Some("0.0.5"),
            true
        ));
        assert!(!agentseek_update_available("AGENTSEEK v0.0.4", None, true));
        assert!(!agentseek_update_available(
            "AGENTSEEK v0.0.4",
            Some("0.0.5"),
            false
        ));
    }
    #[test]
    fn available_agentseek_update_is_included_in_the_install_plan() {
        let status = CliStatus {
            uv_compatible: true,
            node_compatible: true,
            npm_compatible: true,
            cli_compatible: true,
            cli_update_available: true,
            ..CliStatus::default()
        };

        assert_eq!(required_runtime_dependencies(&status), ["agentseek"]);
    }
    #[test]
    fn runtime_install_scripts_use_platform_installers_without_mutating_system_uv() {
        let requirements: RuntimeRequirements =
            serde_json::from_str(DEFAULT_RUNTIME_REQUIREMENTS).expect("parse requirements");
        let status = CliStatus {
            uv_available: true,
            uv_path: "/usr/local/bin/uv".to_string(),
            node_compatible: true,
            npm_compatible: true,
            cli_compatible: true,
            ..CliStatus::default()
        };
        let task_dir = Path::new("/tmp/agentseek-install-task");
        let runtime_root = Path::new("/tmp/agentseek-runtime");

        let posix = posix_runtime_install_script(&requirements, &status, task_dir, runtime_root);
        assert!(posix.contains("https://astral.sh/uv/install.sh"));
        assert!(posix.contains("$HOME/.local/bin/uv"));
        assert!(posix.contains("--output \"$installer_file.tmp\""));
        assert!(posix.contains("bash -n \"$installer_file.tmp\""));
        assert!(!posix.contains("| sh"));
        assert!(!posix.contains("uv self update"));
        assert!(!posix.contains("export METHOD=script"));
        assert!(!posix.contains(
            "Installation completed. AgentSeek Desktop will recheck automatically. Press Enter"
        ));
        assert!(!posix
            .chars()
            .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character)));
        #[cfg(unix)]
        {
            let script_path =
                env::temp_dir().join(format!("agentseek-install-{}.command", unique_stamp()));
            fs::write(&script_path, &posix).expect("write generated POSIX installer");
            let output = std::process::Command::new("bash")
                .arg("-n")
                .arg(&script_path)
                .output()
                .expect("validate generated POSIX installer");
            fs::remove_file(script_path).expect("remove generated POSIX installer");
            assert!(
                output.status.success(),
                "generated installer is invalid: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let node_status = CliStatus {
            uv_available: true,
            uv_path: "/usr/local/bin/uv".to_string(),
            node_compatible: false,
            npm_compatible: false,
            cli_compatible: true,
            ..CliStatus::default()
        };
        let node_posix =
            posix_runtime_install_script(&requirements, &node_status, task_dir, runtime_root);
        assert!(node_posix.contains("export NVM_METHOD=script"));
        assert!(node_posix.contains("export GIT_TERMINAL_PROMPT=0"));
        assert!(!node_posix.contains("gitee.com"));
        assert!(!node_posix.contains("NVM_SOURCE"));

        let windows =
            windows_runtime_install_script(&requirements, &status, task_dir, runtime_root);
        assert!(windows.contains("https://astral.sh/uv/install.ps1"));
        assert!(windows.contains("Invoke-DownloadWithRetry"));
        assert!(!windows.contains("https://astral.sh/uv/install.sh"));
        assert!(!windows.contains("Read-Host"));
        assert!(!windows.contains("-NoExit"));
        assert!(!windows
            .chars()
            .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character)));
    }
    #[test]
    fn managed_node_install_uses_private_nvm_and_bundled_npm() {
        let requirements: RuntimeRequirements =
            serde_json::from_str(DEFAULT_RUNTIME_REQUIREMENTS).expect("parse requirements");
        let commands = dependency_commands(
            &requirements,
            "macos",
            Some(Path::new("/tmp/agentseek-runtime")),
            true,
            false,
        );
        let command = commands.get("node").expect("node install command");

        assert!(command.contains("NVM_DIR=\"/tmp/agentseek-runtime/nvm\""));
        assert!(command.contains("nvm install 24"));
        assert!(command.contains("node --version && npm --version"));
        assert!(!command.contains("install -g npm"));
    }
}
