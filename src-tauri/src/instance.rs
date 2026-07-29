// Instance lifecycle: target validation, environment application,
// process spawning, deployment, and process management.

fn validate_target(target: &Path) -> Result<(), String> {
    if target.exists() {
        if !target.is_dir() {
            return Err("Target path is not a directory".to_string());
        }
        if target
            .read_dir()
            .map_err(|error| error.to_string())?
            .next()
            .is_some()
        {
            return Err("Target directory must be empty".to_string());
        }
    }
    Ok(())
}

fn instance_target_path(parent: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Instance name cannot be empty".to_string());
    }
    if matches!(name, "." | "..") || name.contains(['/', '\\', '\0']) {
        return Err("Instance name cannot contain path separators".to_string());
    }
    Ok(parent.join(name))
}

fn find_env_example(root: &Path) -> Option<PathBuf> {
    let direct = root.join(".env.example");
    if direct.is_file() {
        return Some(direct);
    }
    fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let path = entry.path();
            path.is_dir()
                .then(|| path.join(".env.example"))
                .filter(|candidate| candidate.is_file())
        })
}

/// Recursively search for a file by name, up to `max_depth` levels deep.
fn find_file_recursive(root: &Path, filename: &str, max_depth: usize) -> Option<PathBuf> {
    if max_depth == 0 {
        return None;
    }
    for entry in fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|n| n.to_str() == Some(filename)) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_file_recursive(&path, filename, max_depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Patch convert_models.py in agentic-rag-openvino instances to use
/// `optimum-cli export openvino` instead of `python -m optimum.exporters.openvino`.
fn patch_convert_models_if_needed(instance_dir: &Path) {
    let Some(path) = find_file_recursive(instance_dir, "convert_models.py", 5) else {
        return;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    let old = r#"sys.executable, "-m", "optimum.exporters.openvino","#;
    let new = r#""optimum-cli", "export", "openvino","#;
    if !content.contains(old) {
        return;
    }
    let patched = content.replacen(old, new, 1);
    let _ = fs::write(&path, patched);
}

/// Patch agent.py in agentic-rag-openvino instances to add an async
/// compatibility shim for HuggingFacePipeline.
fn patch_agent_async_if_needed(instance_dir: &Path) {
    let Some(path) = find_file_recursive(instance_dir, "agent.py", 5) else {
        return;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    if content.contains("_patched_agenerate") {
        return;
    }
    let marker = "from langchain_oceanbase.vectorstores import OceanbaseVectorStore";
    if !content.contains(marker) {
        return;
    }
    let shim = "\n# --- Async compatibility shim for HuggingFacePipeline ---\n# ChatHuggingFace._astream doesn't check for HuggingFacePipeline and tries\n# to access async_client (which only exists on HuggingFaceEndpoint).\n# Route async calls through asyncio.to_thread to the sync _generate method.\nimport asyncio\nfrom langchain_core.messages import AIMessageChunk\nfrom langchain_core.outputs import ChatGenerationChunk as _ChatGenChunk\ntry:\n    from langchain_huggingface.chat_models.huggingface import ChatHuggingFace\n    from langchain_huggingface.chat_models.huggingface import HuggingFacePipeline\nexcept ImportError:\n    try:\n        from langchain_community.chat_models.huggingface import ChatHuggingFace\n        from langchain_community.llms.huggingface_pipeline import HuggingFacePipeline\n    except ImportError:\n        ChatHuggingFace = None\n        HuggingFacePipeline = None\n\nif ChatHuggingFace is not None:\n    _orig_agenerate = ChatHuggingFace._agenerate\n    _orig_astream = ChatHuggingFace._astream\n\n    async def _patched_agenerate(self, messages, stop=None, run_manager=None, stream=None, **kwargs):\n        if isinstance(self.llm, HuggingFacePipeline):\n            return await asyncio.to_thread(self._generate, messages, stop, run_manager, **kwargs)\n        return await _orig_agenerate(self, messages, stop, run_manager, stream, **kwargs)\n\n    async def _patched_astream(self, messages, stop=None, run_manager=None, *, stream_usage=None, **kwargs):\n        if isinstance(self.llm, HuggingFacePipeline):\n            result = await asyncio.to_thread(self._generate, messages, stop, run_manager, **kwargs)\n            for gen in result.generations:\n                yield _ChatGenChunk(message=AIMessageChunk(content=gen.text), generation_info=gen.generation_info)\n            return\n        async for chunk in _orig_astream(self, messages, stop, run_manager, stream_usage=stream_usage, **kwargs):\n            yield chunk\n\n    ChatHuggingFace._agenerate = _patched_agenerate\n    ChatHuggingFace._astream = _patched_astream\n";
    let patched = content.replacen(marker, &format!("{shim}\n{marker}"), 1);
    let _ = fs::write(&path, patched);
}

/// Patch Dockerfile to add apt mirror fallback.
fn patch_dockerfile_apt_mirror_if_needed(instance_dir: &Path) {
    let Some(path) = find_file_recursive(instance_dir, "Dockerfile", 5) else {
        return;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    if content.contains("mirrors.aliyun.com") || !content.contains("apt-get update") {
        return;
    }
    let old = "apt-get update";
    let new = "(timeout 60 apt-get update -o Acquire::http::Timeout=10 -o Acquire::https::Timeout=10 -o Acquire::Retries=1 && [ -n \"$(find /var/lib/apt/lists -name '*Packages*' 2>/dev/null)\" ] || (sed -i 's|deb.debian.org|mirrors.aliyun.com|g; s|security.debian.org|mirrors.aliyun.com|g' /etc/apt/sources.list.d/debian.sources 2>/dev/null; sed -i 's|deb.debian.org|mirrors.aliyun.com|g; s|security.debian.org|mirrors.aliyun.com|g' /etc/apt/sources.list 2>/dev/null; apt-get update))";
    let patched = content.replacen(old, new, 1);
    let _ = fs::write(&path, patched);
}

/// Patch Dockerfile to add GitHub and PyPI mirror fallbacks for slow
/// connections in China.
///
/// Two independent checks:
/// 1. **GitHub mirror** – read `pyproject.toml`, find the first GitHub repo,
///    download 500 KB of its tarball, must finish in 3 s (≈167 KB/s).  If too
///    slow, route all GitHub URLs through `ghfast.top` proxy.
/// 2. **PyPI mirror** – download 200 KB from pypi.org, must finish in 3 s
///    (≈67 KB/s).  If too slow, fall back to `mirrors.aliyun.com/pypi/simple/`.
fn patch_dockerfile_mirrors_if_needed(instance_dir: &Path) {
    let Some(path) = find_file_recursive(instance_dir, "Dockerfile", 5) else {
        return;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    // Skip if already patched (ghfast.top is the definitive marker) or no uv
    // sync command.
    if content.contains("ghfast.top") || !content.contains("uv sync") {
        return;
    }
    // Replace the UV index variable handling block with: GitHub mirror test +
    // PyPI speed test + Aliyun mirror fallback.
    //
    // Templates use this pattern:
    //   `if [ -n "${UV_DEFAULT_INDEX:-}" ]; then export UV_DEFAULT_INDEX; fi; \
    //    if [ -n "${UV_INDEX_URL:-}" ]; then export UV_INDEX_URL; fi`
    //
    // We match on the common prefix `if [ -n "${UV_DEFAULT_INDEX:-}"` and
    // rebuild everything from that point up to the `UV_INSECURE_HOST` /
    // `UV_LINK_MODE` lines.
    let marker = "if [ -n \"${UV_DEFAULT_INDEX:-}\"";
    let Some(pos) = content.find(marker) else {
        return;
    };
    // Find the end of this logical block: the `fi; \` or `fi` line that
    // precedes either `UV_INSECURE_HOST` or `UV_LINK_MODE`.
    let rest = &content[pos..];
    let end_markers = ["if [ -n \"${UV_INSECURE_HOST", "UV_LINK_MODE"];
    let mut end_pos = rest.len();
    for em in &end_markers {
        if let Some(ep) = rest.find(em) {
            end_pos = end_pos.min(ep);
        }
    }

    let new_block = r#"timeout 15 python -c "import urllib.request,time,sys; c=open('pyproject.toml').read(); i=c.find('github.com/'); sys.exit(0) if i<0 else None; r=c[i+11:].split('\"')[0]; r=r[:-4] if r.endswith('.git') else r; s=time.time(); urllib.request.urlopen(f'https://github.com/{r}/archive/HEAD.tar.gz',timeout=10).read(500000); sys.exit(1 if time.time()-s>3 else 0)" >/dev/null 2>&1 || \
        git config --global url."https://ghfast.top/https://github.com/".insteadOf "https://github.com/"; \
    if [ -n "${UV_DEFAULT_INDEX:-}" ]; then export UV_DEFAULT_INDEX; \
    elif [ -n "${UV_INDEX_URL:-}" ]; then export UV_INDEX_URL; \
    elif ! timeout 15 python -c "import urllib.request,time; start=time.time(); resp=urllib.request.urlopen('https://pypi.org/simple/pip/',timeout=5); resp.read(200000); assert time.time()-start<=3" >/dev/null 2>&1; then \
        export UV_INDEX_URL=https://mirrors.aliyun.com/pypi/simple/; \
    fi; \
    "#;
    let patched = format!("{}{}{}", &content[..pos], new_block, &rest[end_pos..]);
    let _ = fs::write(&path, patched);
}

/// Patch langgraph.json CORS to allow any origin.
///
/// Templates hardcode a specific port (e.g. 5175) in the `http.cors`
/// section of `langgraph.json`.  When `agentseek dev` assigns a different
/// frontend port, the browser blocks cross-origin requests because the
/// Origin header no longer matches `allow_origins` / `allow_origin_regex`.
/// We replace the restrictive entries with a permissive regex so CORS works
/// regardless of the dynamically assigned port.
fn patch_langgraph_cors_if_needed(instance_dir: &Path) {
    let Some(path) = find_file_recursive(instance_dir, "langgraph.json", 5) else {
        return;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };
    let Some(http) = json.get_mut("http").and_then(|v| v.get_mut("cors")) else {
        return;
    };
    let Some(obj) = http.as_object_mut() else {
        return;
    };
    // Replace port-specific allow_origins / allow_origin_regex with a
    // permissive regex that matches any origin.
    obj.insert(
        "allow_origin_regex".to_string(),
        serde_json::json!("^https?://.*$"),
    );
    obj.remove("allow_origins");
    obj.insert("allow_methods".to_string(), serde_json::json!(["*"]));
    obj.insert("allow_headers".to_string(), serde_json::json!(["*"]));
    let Ok(pretty) = serde_json::to_string_pretty(&json) else {
        return;
    };
    let _ = fs::write(&path, format!("{pretty}\n"));
}

fn port_change_details(changes: &[PortChange]) -> String {
    changes
        .iter()
        .map(|change| format!("{}: {} -> {}", change.key, change.old_port, change.new_port))
        .collect::<Vec<_>>()
        .join("\n")
}

fn recheck_instance_ports(
    state: &DesktopState,
    instance: &InstanceRecord,
) -> Result<Vec<PortChange>, String> {
    let env_path = instance
        .env_path
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| "Instance .env has not been generated yet".to_string())?;
    let mut entries = parse_env(&fs::read_to_string(&env_path).map_err(|error| error.to_string())?);
    let _reserved = collect_assigned_ports(state, Some(&instance.id));
    let changes = resolve_port_conflicts(&mut entries)?;
    sync_env_urls_from_lifecycle(&instance.work_dir, &mut entries);
    if !changes.is_empty() || entries.iter().any(|e| e.modified) {
        fs::write(&env_path, render_env(&entries)).map_err(|error| error.to_string())?;
    }
    let root = PathBuf::from(&instance.work_dir);
    let mut synchronized = synchronize_instance_project_name(&root, &instance.name)?
        .into_iter()
        .collect::<Vec<_>>();
    for path in synchronize_instance_port_configs(&root, &entries)? {
        if !synchronized.contains(&path) {
            synchronized.push(path);
        }
    }
    if changes.is_empty() {
        if !synchronized.is_empty() {
            state.log(
                Some(&instance.id),
                &instance.name,
                "config",
                "info",
                format!(
                    "Runtime configs updated based on instance .env\n{}",
                    synchronized
                        .iter()
                        .map(|path| format!("  {}", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                None,
            );
        }
        return Ok(changes);
    }
    {
        let mut data = state
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?;
        for entry in entries.iter().filter(|entry| entry.modified) {
            if let Some(saved) = data.vault.iter_mut().find(|saved| saved.key == entry.key) {
                saved.value = entry.value.clone();
                saved.comment = entry.comment.clone();
                saved.source = "instance".to_string();
                saved.modified = false;
            } else {
                let mut saved = entry.clone();
                saved.source = "instance".to_string();
                saved.modified = false;
                data.vault.push(saved);
            }
        }
    }
    state.persist_current_vault()?;
    state.log(
        Some(&instance.id),
        &instance.name,
        "config",
        "warning",
        format!(
            "Pre-deployment port recheck found local port conflicts; ports reassigned and synced to instance runtime configs and env vault\nPort changes:\n{}\nSynced files:\n  {}\n{}",
            port_change_details(&changes),
            env_path.display(),
            synchronized
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        None,
    );
    Ok(changes)
}

fn instance_by_id(state: &DesktopState, instance_id: &str) -> Result<InstanceRecord, String> {
    state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?
        .instances
        .iter()
        .find(|instance| instance.id == instance_id)
        .cloned()
        .ok_or_else(|| "Instance not found".to_string())
}

fn update_instance(state: &DesktopState, record: InstanceRecord) -> Result<(), String> {
    state.persist_instance(&record)?;
    let mut data = state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?;
    let existing = data
        .instances
        .iter_mut()
        .find(|instance| instance.id == record.id)
        .ok_or_else(|| "Instance not found".to_string())?;
    *existing = record.clone();
    Ok(())
}

fn parse_describe_ports(output: &str) -> HashMap<String, u16> {
    let mut ports = HashMap::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            if key.to_ascii_lowercase().ends_with("_port") {
                if let Ok(port) = value.trim().parse::<u16>() {
                    if port > 0 {
                        ports.insert(key.to_string(), port);
                    }
                }
            }
        }
    }
    ports
}

fn resolve_describe_ports(
    describe_output: &str,
    reserved: &HashSet<u16>,
) -> Result<(HashMap<String, u16>, Vec<PortChange>), String> {
    let defaults = parse_describe_ports(describe_output);
    let mut resolved = HashMap::new();
    let mut changes = Vec::new();
    let mut taken: HashSet<u16> = reserved.iter().copied().collect();
    for (key, default_port) in &defaults {
        let env_key = key.to_ascii_uppercase();
        let port = if port_is_available(*default_port) && taken.insert(*default_port) {
            *default_port
        } else {
            let mut replacement = available_ephemeral_port()?;
            while taken.contains(&replacement) {
                replacement = available_ephemeral_port()?;
            }
            taken.insert(replacement);
            changes.push(PortChange {
                key: env_key.clone(),
                old_port: *default_port,
                new_port: replacement,
            });
            replacement
        };
        resolved.insert(env_key, port);
    }
    Ok((resolved, changes))
}

static HF_ENDPOINT_REACHABLE: OnceLock<bool> = OnceLock::new();

/// Probe whether huggingface.co is reachable (TCP connect to port 443).
fn huggingface_reachable() -> bool {
    *HF_ENDPOINT_REACHABLE.get_or_init(|| {
        "huggingface.co:443"
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map_or(false, |addr| {
                TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok()
            })
    })
}

fn apply_instance_environment(
    command: &mut Command,
    instance: &InstanceRecord,
) -> Result<Vec<EnvVariable>, String> {
    let path = instance
        .env_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&instance.work_dir).join(".env"));
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let entries = parse_env(
        &fs::read_to_string(&path)
            .map_err(|error| format!("Failed to read instance env file {}: {error}", path.display()))?,
    );
    for entry in &entries {
        if entry.key.contains(['=', '\0']) || entry.value.contains('\0') {
            return Err(format!("Instance .env contains invalid variable: {}", entry.key));
        }
        command.env(&entry.key, process_env_value(&entry.value));
    }
    // Auto-inject VITE_LANGGRAPH_API_URL so Vite frontends can connect to the LangGraph backend.
    if !entries
        .iter()
        .any(|e| e.key.to_ascii_uppercase() == "VITE_LANGGRAPH_API_URL")
    {
        // Prefer BACKEND_PORT (the actual langgraph dev --port) over LANGGRAPH_PORT
        // (which may be a stale cookiecutter variable unrelated to the running process).
        let langgraph_port = entries
            .iter()
            .find(|e| e.key.to_ascii_uppercase() == "BACKEND_PORT")
            .or_else(|| entries.iter().find(|e| e.key.to_ascii_uppercase() == "LANGGRAPH_PORT"))
            .and_then(|e| e.value.trim().parse::<u16>().ok());
        if let Some(port) = langgraph_port {
            command.env("VITE_LANGGRAPH_API_URL", format!("http://127.0.0.1:{port}"));
        }
    }
    // Auto-inject HF_ENDPOINT mirror when huggingface.co is unreachable.
    if !entries
        .iter()
        .any(|e| e.key.to_ascii_uppercase() == "HF_ENDPOINT")
    {
        if !huggingface_reachable() {
            command.env("HF_ENDPOINT", "https://hf-mirror.com");
        }
    }
    Ok(entries)
}

fn run_and_log(
    state: &DesktopState,
    instance: &InstanceRecord,
    args: &[&str],
    category: &str,
) -> Result<CommandResult, String> {
    let started = Instant::now();
    let (program, prefix) = cli_parts();
    let printable = std::iter::once(program.as_str())
        .chain(prefix.iter().map(String::as_str))
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    state.log(
        Some(&instance.id),
        &instance.name,
        category,
        "info",
        "Starting command execution",
        Some(printable.clone()),
    );

    let mut command = configured_command(&program);
    apply_instance_environment(&mut command, instance)?;
    command
        .env("NPM_CONFIG_PREFER_OFFLINE", "true")
        .env("NPM_CONFIG_AUDIT", "false")
        .env("NPM_CONFIG_FUND", "false")
        .env("NPM_CONFIG_UPDATE_NOTIFIER", "false")
        .env("TQDM_DISABLE", "1")
        .args(&prefix)
        .args(args)
        .current_dir(&instance.work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        let message = format!("Failed to execute {printable}: {error}");
        state.log(
            Some(&instance.id),
            &instance.name,
            category,
            "error",
            &message,
            Some(printable.clone()),
        );
        message
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_state = state.clone();
    let stdout_id = instance.id.clone();
    let stdout_name = instance.name.clone();
    let stdout_category = category.to_string();
    let stdout_handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("sha256:") && line.contains(" / ") && !line.contains("done") {
                    continue;
                }
                stdout_state.log(
                    Some(&stdout_id),
                    &stdout_name,
                    &stdout_category,
                    "info",
                    &line,
                    None,
                );
                output.push(line);
            }
        }
        output
    });
    let stderr_state = state.clone();
    let stderr_id = instance.id.clone();
    let stderr_name = instance.name.clone();
    let stderr_category = category.to_string();
    let stderr_handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.contains("sha256:") && line.contains(" / ") && !line.contains("done") {
                    continue;
                }
                stderr_state.log(
                    Some(&stderr_id),
                    &stderr_name,
                    &stderr_category,
                    "info",
                    &line,
                    None,
                );
                output.push(line);
            }
        }
        output
    });

    let status = child
        .wait()
        .map_err(|error| format!("Failed to wait for command: {printable}: {error}"))?;
    let stdout_lines = stdout_handle.join().unwrap_or_default();
    let stderr_lines = stderr_handle.join().unwrap_or_default();
    let output = stdout_lines
        .into_iter()
        .chain(stderr_lines)
        .collect::<Vec<_>>()
        .join("\n");
    let code = status.code().unwrap_or(1);
    state.log(
        Some(&instance.id),
        &instance.name,
        category,
        if code == 0 { "success" } else { "error" },
        if code == 0 {
            format!(
                "Command completed: {printable} ({} seconds)",
                started.elapsed().as_secs()
            )
        } else {
            format!(
                "Command failed: {printable} ({} seconds)",
                started.elapsed().as_secs()
            )
        },
        None,
    );
    if code != 0 {
        return Err(if output.is_empty() {
            format!("Command execution failed: {printable}")
        } else {
            output
        });
    }
    Ok(CommandResult {
        code,
        output,
        command: printable,
    })
}

fn run_instance_cli(instance: &InstanceRecord, args: &[&str]) -> Result<CommandResult, String> {
    let (program, prefix) = cli_parts();
    let printable = std::iter::once(program.as_str())
        .chain(prefix.iter().map(String::as_str))
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = configured_command(&program);
    apply_instance_environment(&mut command, instance)?;
    let output = command
        .args(&prefix)
        .args(args)
        .current_dir(&instance.work_dir)
        .output()
        .map_err(|error| format!("Failed to execute {printable}: {error}"))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(CommandResult {
        code: output.status.code().unwrap_or(1),
        output: combined.trim().to_string(),
        command: printable,
    })
}

fn wait_for_instance_ready(state: &DesktopState, instance: &InstanceRecord) -> Result<(), String> {
    const READY_TIMEOUT: Duration = Duration::from_secs(300);
    const RETRY_INTERVAL: Duration = Duration::from_secs(2);
    let started = Instant::now();
    let mut latest = String::new();
    state.log(
        Some(&instance.id),
        &instance.name,
        "install",
        "info",
        "Instance process started; waiting for lifecycle health checks to pass",
        Some("agentseek doctor --live".to_string()),
    );
    while started.elapsed() < READY_TIMEOUT {
        if instance.pid.is_some_and(|pid| !process_exists(pid)) {
            // Parent process exited — but child processes (uvicorn/langgraph)
            // may still be running. Try a doctor check before declaring failure.
            let result = run_instance_cli(instance, &["doctor", "--live"])?;
            if result.code == 0 {
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "success",
                    format!(
                        "All lifecycle services ready ({} seconds)",
                        started.elapsed().as_secs()
                    ),
                    Some(result.command),
                );
                return Ok(());
            }
            let (log_path, _) = runtime_log_spool_paths(&state.data_dir, &instance.id);
            let tail = read_runtime_log_tail(&log_path, 80);
            if !tail.is_empty() {
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "error",
                    format!("Process output (last 80 lines):\n{}", tail),
                    None,
                );
            }
            return Err(format!(
                "{} instance startup process exited, please check lifecycle logs",
                instance.name
            ));
        }
        let result = run_instance_cli(instance, &["doctor", "--live"])?;
        latest = result.output;
        if result.code == 0 {
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "success",
                format!(
                    "All lifecycle services ready ({} seconds)",
                    started.elapsed().as_secs()
                ),
                Some(result.command),
            );
            return Ok(());
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
    let latest = latest
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let (log_path, _) = runtime_log_spool_paths(&state.data_dir, &instance.id);
    let tail = read_runtime_log_tail(&log_path, 80);
    if !tail.is_empty() {
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            "error",
            format!("Process output (last 80 lines):\n{}", tail),
            None,
        );
    }
    Err(format!(
        "Timed out waiting for instance services to be ready (300s). Please check lifecycle logs. Last doctor check:\n{latest}"
    ))
}

fn apply_info_urls(instance: &mut InstanceRecord, output: &str) {
    for line in output.lines() {
        let lower = line.to_lowercase();
        let Some(url_start) = lower.find("http") else {
            continue;
        };
        let url = line[url_start..].trim().to_string();
        if lower.contains("studio") || lower.contains("langsmith") {
            instance.studio_url = Some(url);
        } else if lower.contains("frontend") || lower.contains(" ui") {
            instance.ui_url = Some(url);
        } else if lower.contains("agent")
            || lower.contains("gateway")
            || instance.agent_url.is_none()
        {
            instance.agent_url = Some(url);
        }
    }
}

fn ensure_docker_compose_ready(
    state: &DesktopState,
    instance: &InstanceRecord,
    category: &str,
) -> Result<(), String> {
    if let Some(message) = docker_compose_check(Path::new(&instance.work_dir)) {
        state.log(
            Some(&instance.id),
            &instance.name,
            category,
            "error",
            &message,
            Some("docker --version && docker compose version --short && docker info".to_string()),
        );
        return Err(format!("{} instance startup process exited, please check lifecycle logs", instance.name));
    }
    Ok(())
}

fn spawn_instance(state: &DesktopState, instance: &mut InstanceRecord) -> Result<(), String> {
    ensure_docker_compose_ready(state, instance, "install")?;
    if instance.deployment_mode == "docker" {
        let output = configured_command("docker")
            .args(["compose", "up", "-d"])
            .current_dir(&instance.work_dir)
            .output()
            .map_err(|error| format!("Failed to execute Docker Compose: {error}"))?;
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            if output.status.success() {
                "success"
            } else {
                "error"
            },
            message,
            Some("docker compose up -d".to_string()),
        );
        if !output.status.success() {
            return Err("Docker Compose failed to start".to_string());
        }
        instance.pid = None;
        return Ok(());
    }

    let (program, prefix) = cli_parts();
    if let Ok(mut storage) = state.storage.lock() {
        let _ = storage.delete_runtime_logs(&instance.id);
    }
    if let Ok(mut data) = state.data.lock() {
        data.logs.retain(|log| {
            !(log.instance_id.as_deref() == Some(instance.id.as_str())
                && log.category == "runtime")
        });
    }
    let (stdout, stderr) = prepare_runtime_log_spool(state, &instance.id)?;
    let mut command = configured_command(&program);
    let environment = apply_instance_environment(&mut command, instance)?;
    command
        .args(&prefix)
        .args(["dev"])
        .current_dir(&instance.work_dir)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let environment_summary = runtime_environment_summary(&environment);
    if !environment_summary.is_empty() {
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            "info",
            format!(
                "Instance .env injected into startup process; addresses below override lifecycle default ports\n{environment_summary}"
            ),
            None,
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        remove_runtime_log_spool(state, &instance.id);
        format!(
            "Failed to start instance: cannot execute {} (working directory: {}): {}",
            program, instance.work_dir, error
        )
    })?;
    instance.pid = Some(child.id());
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn child_process_ids(parent: u32) -> Vec<u32> {
    let output = Command::new("pgrep")
        .args(["-P", &parent.to_string()])
        .output();
    let direct = output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut descendants = Vec::new();
    for child in direct {
        descendants.push(child);
        descendants.extend(child_process_ids(child));
    }
    descendants
}

fn endpoint_port(url: &str) -> Option<u16> {
    let authority = url.split_once("://")?.1.split('/').next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

fn listener_process_ids(instance: &InstanceRecord) -> Vec<u32> {
    let mut pids = Vec::new();
    for port in instance
        .service_endpoints
        .iter()
        .filter_map(|endpoint| endpoint_port(&endpoint.url))
        .collect::<HashSet<_>>()
    {
        let selector = format!("-iTCP:{port}");
        if let Ok(output) = Command::new("lsof")
            .args(["-nP", "-t", &selector, "-sTCP:LISTEN"])
            .output()
        {
            pids.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| line.trim().parse::<u32>().ok()),
            );
        }
    }
    pids
}

fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn signal_process(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Debug)]
struct StoppedProcess {
    pid: u32,
    executable: String,
}

fn process_executable(pid: u32) -> String {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown process".to_string())
}

fn terminate_processes(
    roots: impl IntoIterator<Item = u32>,
) -> Result<Vec<StoppedProcess>, String> {
    let mut pids = HashSet::new();
    for root in roots {
        pids.insert(root);
        pids.extend(child_process_ids(root));
    }
    let mut ordered = pids.into_iter().collect::<Vec<_>>();
    ordered.sort_unstable();
    ordered.retain(|pid| process_exists(*pid));
    let stopped = ordered
        .iter()
        .map(|pid| StoppedProcess {
            pid: *pid,
            executable: process_executable(*pid),
        })
        .collect::<Vec<_>>();
    for pid in ordered.iter().rev().copied() {
        signal_process(pid, "-TERM");
    }
    std::thread::sleep(Duration::from_millis(800));
    let remaining = ordered
        .iter()
        .copied()
        .filter(|pid| process_exists(*pid))
        .collect::<Vec<_>>();
    for pid in remaining.iter().rev().copied() {
        signal_process(pid, "-KILL");
    }
    if !remaining.is_empty() {
        std::thread::sleep(Duration::from_millis(250));
    }
    let still_running = remaining
        .into_iter()
        .filter(|pid| process_exists(*pid))
        .collect::<Vec<_>>();
    if still_running.is_empty() {
        Ok(stopped)
    } else {
        Err(format!("Failed to stop the following processes: {still_running:?}"))
    }
}

fn stop_instance_process(
    state: &DesktopState,
    instance: &InstanceRecord,
    log_category: &str,
) -> Result<Vec<StoppedProcess>, String> {
    if instance.deployment_mode == "docker" {
        let output = Command::new("docker")
            .args(["compose", "down"])
            .current_dir(&instance.work_dir)
            .output()
            .map_err(|error| format!("Failed to execute Docker Compose: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).to_string());
        }
        Ok(Vec::new())
    } else {
        let mut roots = listener_process_ids(instance);
        if let Some(pid) = instance.pid {
            roots.push(pid);
        }
        let stopped = terminate_processes(roots)?;
        let process_details = if stopped.is_empty() {
            "  No running associated processes found".to_string()
        } else {
            stopped
                .iter()
                .map(|process| format!("  PID {}  {}", process.pid, process.executable))
                .collect::<Vec<_>>()
                .join("\n")
        };
        state.log(
            Some(&instance.id),
            &instance.name,
            log_category,
            "info",
            format!(
                "Instance associated processes stopped\nWorking directory: {}\nProcess count: {}\nDetails:\n{}",
                instance.work_dir,
                stopped.len(),
                process_details
            ),
            None,
        );
        Ok(stopped)
    }
}

fn remove_instance_work_dir(work_dir: &str) -> Result<(), String> {
    let path = PathBuf::from(work_dir);
    if !path.exists() {
        return Ok(());
    }
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| format!("Failed to check instance working directory: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("Refusing to delete symlink instance working directory".to_string());
    }
    if !metadata.is_dir() {
        return Err("Instance working directory is not a directory".to_string());
    }
    let canonical =
        fs::canonicalize(&path).map_err(|error| format!("Failed to resolve instance working directory: {error}"))?;
    if canonical.parent().is_none() {
        return Err("Refusing to delete filesystem root".to_string());
    }
    if let Some(home) = env::var_os("HOME") {
        if fs::canonicalize(home).is_ok_and(|home| home == canonical) {
            return Err("Refusing to delete user home directory".to_string());
        }
    }
    if env::current_dir()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok())
        .is_some_and(|current| current == canonical)
    {
        return Err("Refusing to delete AgentSeek Desktop current working directory".to_string());
    }
    fs::remove_dir_all(&canonical).map_err(|error| format!("Failed to delete instance working directory: {error}"))
}

fn docker_compose_file(project_dir: &Path) -> Option<PathBuf> {
    [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ]
    .iter()
    .map(|name| project_dir.join(name))
    .find(|path| path.is_file())
}

fn docker_compose_check(project_dir: &Path) -> Option<String> {
    let compose_file = docker_compose_file(project_dir)?;
    let docker_status = check_docker();
    let mut missing = Vec::new();
    if !docker_status.cli_available {
        missing.push("Docker CLI not installed");
    }
    if !docker_status.compose_v2_available {
        missing.push("Docker Compose V2 not installed");
    }
    if !docker_status.daemon_running {
        missing.push("Docker not started");
    }
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "Project contains {}, but{}. Please install and start Docker before continuing.",
            compose_file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("docker-compose.yml"),
            missing.join(", ")
        ))
    }
}

fn check_docker() -> DockerStatus {
    let cli_available = configured_command("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let compose_v2_available = cli_available
        && configured_command("docker")
            .args(["compose", "version", "--short"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    let daemon_running = cli_available
        && configured_command("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    DockerStatus {
        cli_available,
        compose_v2_available,
        daemon_running,
    }
}
