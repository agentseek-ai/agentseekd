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

/// Replace `old_port` with `new_port` only when the digit sequence appears
/// in a port-like context: preceded by `:` (URL/JSON), a `*_PORT=` key
/// assignment, or a standalone `port` keyword. This avoids replacing the
/// same digits inside unrelated strings, IDs, or comments.
fn replace_port_in_context(content: &str, old_port: u16, new_port: u16) -> String {
    let old = old_port.to_string();
    let new = new_port.to_string();
    let mut result = String::with_capacity(content.len());
    let mut remaining = content;
    while let Some(pos) = remaining.find(&old) {
        let before = &remaining[..pos];
        let after = &remaining[pos + old.len()..];
        // Ensure the match is a whole number (not part of a larger digit sequence).
        let prev_char = before.chars().last();
        let next_char = after.chars().next();
        let is_boundary = !prev_char.is_some_and(|c| c.is_ascii_digit())
            && !next_char.is_some_and(|c| c.is_ascii_digit());
        if !is_boundary {
            result.push_str(&remaining[..pos + old.len()]);
            remaining = after;
            continue;
        }
        let trimmed_before = before.trim_end();
        // Port key assignment: `=` preceded by a key ending in `_PORT`
        // (e.g. `PHOENIX_PORT=6006`), so plain `foo=6006` is untouched.
        let port_key_assignment = trimmed_before.ends_with('=') && {
            let key = trimmed_before[..trimmed_before.len() - 1]
                .rsplit(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("")
                .trim_matches(|c| c == '"' || c == '\'');
            key.to_ascii_uppercase().ends_with("_PORT")
        };
        // Standalone `port` keyword bounded by quote/whitespace/line start
        // (e.g. `port 6006`), so words like `import`/`support` are not
        // matched.
        let keyword = trimmed_before
            .strip_suffix('"')
            .or_else(|| trimmed_before.strip_suffix('\''))
            .unwrap_or(trimmed_before);
        let port_keyword = {
            let lower = keyword.to_ascii_lowercase();
            lower
                .rfind("port")
                .is_some_and(|p| p + 4 == lower.len())
                && keyword[..keyword.len().saturating_sub(4)]
                    .chars()
                    .last()
                    .map_or(true, |c| c == '"' || c == '\'' || c.is_whitespace())
        };
        let in_port_context = trimmed_before.ends_with(':') || port_key_assignment || port_keyword;
        if in_port_context {
            result.push_str(before);
            result.push_str(&new);
        } else {
            result.push_str(&remaining[..pos + old.len()]);
        }
        remaining = after;
    }
    result.push_str(remaining);
    result
}

/// Patch source files when port conflicts required different ports than the
/// cookiecutter defaults. After lifecycle.toml URLs are updated, the rendered
/// source files (e.g. `langgraph_dev.py` with a hardcoded port) may still
/// reference the original default port. This function walks the instance
/// directory and replaces old port values with the resolved ones in `.py`,
/// `.ts`, `.js`, and `.json` files.
///
/// Only port numbers appearing in port-like contexts are replaced (e.g. after
/// `:`, `port=`, `PORT=`) to avoid accidentally modifying unrelated numeric
/// literals that happen to share the same digits.
fn patch_source_ports_for_conflicts(instance_dir: &Path, port_changes: &[PortChange]) {
    if port_changes.is_empty() {
        return;
    }

    fn walk(dir: &Path, port_changes: &[PortChange]) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') || name == "node_modules" || name == ".venv" {
                    continue;
                }
                walk(&path, port_changes);
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !["py", "ts", "js", "json"].contains(&ext) {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let mut updated = content.clone();
            for change in port_changes {
                updated = replace_port_in_context(&updated, change.old_port, change.new_port);
            }
            if updated != content {
                let _ = fs::write(&path, updated);
            }
        }
    }

    walk(instance_dir, port_changes);
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

/// Patch agent.py to add an async compatibility shim for HuggingFacePipeline.
///
/// The shim is added only when the instance depends on `langchain-huggingface`
/// (checked via `pyproject.toml`), which provides `ChatHuggingFace` /
/// `HuggingFacePipeline` — the classes affected by the `async_client` bug
/// (langchain issue #34134, closed as not planned).
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
    // Only add the shim when the instance depends on langchain-huggingface,
    // which provides ChatHuggingFace / HuggingFacePipeline (the classes
    // affected by the async_client bug).
    let uses_hf = find_file_recursive(instance_dir, "pyproject.toml", 5)
        .and_then(|p| fs::read_to_string(&p).ok())
        .map(|c| c.contains("langchain-huggingface"))
        .unwrap_or(false);
    if !uses_hf {
        return;
    }
    let marker = "from langchain_oceanbase.vectorstores import OceanbaseVectorStore";
    let shim = "\n# --- Async compatibility shim for HuggingFacePipeline ---\n# ChatHuggingFace._astream doesn't check for HuggingFacePipeline and tries\n# to access async_client (which only exists on HuggingFaceEndpoint).\n# Route async calls through asyncio.to_thread to the sync _generate method.\nimport asyncio\nfrom langchain_core.messages import AIMessageChunk\nfrom langchain_core.outputs import ChatGenerationChunk as _ChatGenChunk\ntry:\n    from langchain_huggingface.chat_models.huggingface import ChatHuggingFace\n    from langchain_huggingface.chat_models.huggingface import HuggingFacePipeline\nexcept ImportError:\n    try:\n        from langchain_community.chat_models.huggingface import ChatHuggingFace\n        from langchain_community.llms.huggingface_pipeline import HuggingFacePipeline\n    except ImportError:\n        ChatHuggingFace = None\n        HuggingFacePipeline = None\n\nif ChatHuggingFace is not None:\n    _orig_agenerate = ChatHuggingFace._agenerate\n    _orig_astream = ChatHuggingFace._astream\n\n    async def _patched_agenerate(self, messages, stop=None, run_manager=None, stream=None, **kwargs):\n        if isinstance(self.llm, HuggingFacePipeline):\n            return await asyncio.to_thread(self._generate, messages, stop, run_manager, **kwargs)\n        return await _orig_agenerate(self, messages, stop, run_manager, stream, **kwargs)\n\n    async def _patched_astream(self, messages, stop=None, run_manager=None, *, stream_usage=None, **kwargs):\n        if isinstance(self.llm, HuggingFacePipeline):\n            result = await asyncio.to_thread(self._generate, messages, stop, run_manager, **kwargs)\n            for gen in result.generations:\n                yield _ChatGenChunk(message=AIMessageChunk(content=gen.text), generation_info=gen.generation_info)\n            return\n        async for chunk in _orig_astream(self, messages, stop, run_manager, stream_usage=stream_usage, **kwargs):\n            yield chunk\n\n    ChatHuggingFace._agenerate = _patched_agenerate\n    ChatHuggingFace._astream = _patched_astream\n";
    let patched = if content.contains(marker) {
        // Insert shim before the OceanbaseVectorStore marker in agent.py.
        content.replacen(marker, &format!("{shim}\n{marker}"), 1)
    } else {
        // No marker in agent.py — prepend shim.
        format!("{shim}\n{content}")
    };
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
    if content.contains(APT_MIRROR) || !content.contains("apt-get update") {
        return;
    }
    let old = "apt-get update";
    let new = format!("(timeout 60 apt-get update -o Acquire::http::Timeout=10 -o Acquire::https::Timeout=10 -o Acquire::Retries=1 && [ -n \"$(find /var/lib/apt/lists -name '*Packages*' 2>/dev/null)\" ] || (sed -i 's|deb.debian.org|{mirror}|g; s|security.debian.org|{mirror}|g' /etc/apt/sources.list.d/debian.sources 2>/dev/null; sed -i 's|deb.debian.org|{mirror}|g; s|security.debian.org|{mirror}|g' /etc/apt/sources.list 2>/dev/null; apt-get update))", mirror = APT_MIRROR);
    let patched = content.replacen(old, &new, 1);
    let _ = fs::write(&path, patched);
}

/// Patch Dockerfile to add PyPI mirror fallback for slow
/// connections in China.
///
/// **PyPI mirror** – download 200 KB from pypi.org, must finish in 3 s
/// (≈67 KB/s).  If too slow, fall back to `mirrors.aliyun.com/pypi/simple/`.
fn patch_dockerfile_mirrors_if_needed(instance_dir: &Path) {
    let Some(path) = find_file_recursive(instance_dir, "Dockerfile", 5) else {
        return;
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    // Skip if already patched or no uv sync command.
    if content.contains("mirrors.aliyun.com/pypi/simple/") || !content.contains("uv sync") {
        return;
    }
    // Replace the UV index variable handling block with:
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

    let new_block = format!(
        r#"if [ -n "${{UV_DEFAULT_INDEX:-}}" ]; then export UV_DEFAULT_INDEX; \
    elif [ -n "${{UV_INDEX_URL:-}}" ]; then export UV_INDEX_URL; \
    elif ! timeout 15 python -c "import urllib.request,time; start=time.time(); resp=urllib.request.urlopen('https://pypi.org/simple/pip/',timeout=5); resp.read(200000); assert time.time()-start<=3" >/dev/null 2>&1; then \
        export UV_INDEX_URL={pypi_mirror}; \
    fi; \
    "#,
        pypi_mirror = PYPI_MIRROR
    );
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
    let reserved = collect_assigned_ports(state, Some(&instance.id));
    // Treat ports reserved by other instances as conflicts even when their
    // processes are stopped; the instance itself owns nothing yet pre-deploy.
    let changes = resolve_port_conflicts_inner(&mut entries, &reserved, &HashSet::new())?;
    // Restore corrupted container-internal endpoints to their loopback
    // template defaults first, then the lifecycle sync below aligns their
    // ports with the resolved values (e.g. 127.0.0.1:6006 -> 127.0.0.1:56438).
    restore_non_loopback_url_defaults(&instance.work_dir, &mut entries);
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
            .is_some_and(|addr| {
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
        .any(|e| e.key.eq_ignore_ascii_case("VITE_LANGGRAPH_API_URL"))
    {
        // Prefer BACKEND_PORT (the actual langgraph dev --port) over LANGGRAPH_PORT
        // (which may be a stale cookiecutter variable unrelated to the running process).
        let langgraph_port = entries
            .iter()
            .find(|e| e.key.eq_ignore_ascii_case("BACKEND_PORT"))
            .or_else(|| entries.iter().find(|e| e.key.eq_ignore_ascii_case("LANGGRAPH_PORT")))
            .and_then(|e| e.value.trim().parse::<u16>().ok());
        if let Some(port) = langgraph_port {
            command.env("VITE_LANGGRAPH_API_URL", format!("http://127.0.0.1:{port}"));
        }
    }
    // Auto-inject HF_ENDPOINT mirror when huggingface.co is unreachable.
    if !entries
        .iter()
        .any(|e| e.key.eq_ignore_ascii_case("HF_ENDPOINT"))
        && !huggingface_reachable() {
            command.env("HF_ENDPOINT", HF_MIRROR);
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
    if docker_compose_file(Path::new(&instance.work_dir)).is_some() {
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

/// Find processes listening on the instance's service ports that belong to the
/// instance process tree (pid + descendants). Unrelated processes that merely
/// happen to use the same port are returned separately and never terminated,
/// preventing accidental kills of other applications.
fn listener_process_ids(instance: &InstanceRecord) -> (Vec<u32>, Vec<(u32, u16)>) {
    let mut related = HashSet::new();
    if let Some(pid) = instance.pid {
        related.insert(pid);
        related.extend(child_process_ids(pid));
    }
    let mut pids = Vec::new();
    let mut skipped = Vec::new();
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
            for pid in String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
            {
                if related.contains(&pid) {
                    pids.push(pid);
                } else {
                    skipped.push((pid, port));
                }
            }
        }
    }
    (pids, skipped)
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
    if docker_compose_file(Path::new(&instance.work_dir)).is_some() {
        let output = Command::new("docker")
            // Stopping an instance must preserve its containers for the next start.
            .args(["compose", "stop"])
            .current_dir(&instance.work_dir)
            .output()
            .map_err(|error| format!("Failed to execute Docker Compose: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).to_string());
        }
        Ok(Vec::new())
    } else {
        let (listener_pids, skipped_listeners) = listener_process_ids(instance);
        let mut roots = listener_pids;
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
        let skipped_details = if skipped_listeners.is_empty() {
            String::new()
        } else {
            format!(
                "\nSkipped unrelated processes listening on instance ports (not part of instance process tree):\n{}",
                skipped_listeners
                    .iter()
                    .map(|(pid, port)| format!("  PID {pid} on port {port}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        state.log(
            Some(&instance.id),
            &instance.name,
            log_category,
            "info",
            format!(
                "Instance associated processes stopped\nWorking directory: {}\nProcess count: {}\nDetails:\n{}{}",
                instance.work_dir,
                stopped.len(),
                process_details,
                skipped_details
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

/// Recursively search for docker compose files up to `max_depth` levels.
fn find_compose_files_recursive(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    if max_depth == 0 {
        return Vec::new();
    }
    let mut found = Vec::new();
    if let Some(file) = docker_compose_file(root) {
        found.push(file);
    }
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            {
                found.extend(find_compose_files_recursive(&path, max_depth - 1));
            }
        }
    }
    found
}

/// Find and remove Docker containers whose compose working directory label
/// matches `work_dir`. This is a fallback for when the compose file has been
/// deleted or is not at the expected location.
fn cleanup_docker_containers_by_label(work_dir: &str) -> Result<usize, String> {
    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=com.docker.compose.project.working_dir",
            "--format",
            "{{.ID}} {{.Label \"com.docker.compose.project.working_dir\"}}",
        ])
        .output();
    let Ok(output) = output else {
        return Ok(0);
    };
    if !output.status.success() {
        return Ok(0);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let matching_ids: Vec<String> = stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let id = parts.next()?.trim().to_string();
            let dir = parts.next().unwrap_or("").trim().to_string();
            if !id.is_empty() && dir == work_dir {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    if matching_ids.is_empty() {
        return Ok(0);
    }
    let mut args = vec!["rm".to_string(), "-f".to_string()];
    args.extend(matching_ids.clone());
    let remove_output = Command::new("docker")
        .args(&args)
        .output()
        .map_err(|error| format!("Failed to execute docker rm: {error}"))?;
    if remove_output.status.success() {
        Ok(matching_ids.len())
    } else {
        let stderr = String::from_utf8_lossy(&remove_output.stderr);
        Err(format!(
            "Failed to remove {} container(s) by label: {}",
            matching_ids.len(),
            stderr.trim()
        ))
    }
}

/// Remove Docker containers associated with an instance.
/// 1. Recursively searches for compose files under the working directory
///    and runs `docker compose down --remove-orphans` for each.
/// 2. Falls back to label-based cleanup: finds containers whose
///    `com.docker.compose.project.working_dir` label matches `work_dir`
///    and removes them via `docker rm -f`.
/// Returns `Ok(true)` if any containers were cleaned up, `Ok(false)` if
/// no compose files or matching containers were found.
fn cleanup_docker_containers(work_dir: &str) -> Result<bool, String> {
    let work_dir_path = Path::new(work_dir);
    let compose_files = find_compose_files_recursive(work_dir_path, 3);
    let mut any_cleaned = false;
    let mut errors: Vec<String> = Vec::new();

    for compose_file in &compose_files {
        let compose_dir = compose_file.parent().unwrap_or(work_dir_path);
        // Compose validates service-level `env_file` entries even for `down`.
        // Use a temporary empty root .env when a broken instance has lost it.
        let env_path = compose_dir.join(".env");
        let temporary_env = !env_path.exists();
        if temporary_env {
            if let Err(error) = fs::write(&env_path, "") {
                errors.push(format!(
                    "Failed to create temporary {} for Docker cleanup: {error}",
                    env_path.display()
                ));
                continue;
            }
        }
        let output = Command::new("docker")
            .args(["compose", "down", "--remove-orphans"])
            .current_dir(compose_dir)
            .output();
        if temporary_env {
            let _ = fs::remove_file(&env_path);
        }
        match output {
            Ok(output) if output.status.success() => {
                any_cleaned = true;
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !(stderr.contains("no configuration file provided")
                    || stderr.contains("no containers to start"))
                {
                    errors.push(format!(
                        "docker compose down in {} failed (exit {}): {}",
                        compose_dir.display(),
                        output.status.code().unwrap_or(-1),
                        stderr.trim()
                    ));
                }
            }
            Err(error) => {
                errors.push(format!(
                    "Failed to execute docker compose down in {}: {error}",
                    compose_dir.display()
                ));
            }
        }
    }

    // Label-based fallback: find containers whose compose working directory
    // matches this instance's work_dir, even if the compose file is gone.
    match cleanup_docker_containers_by_label(work_dir) {
        Ok(count) if count > 0 => {
            any_cleaned = true;
        }
        Ok(_) => {}
        Err(error) => {
            errors.push(error);
        }
    }

    if errors.is_empty() {
        Ok(any_cleaned)
    } else {
        let detail = errors.join("; ");
        let docker_status = check_docker();
        if !docker_status.cli_available || !docker_status.daemon_running {
            Err(format!(
                "{detail}\n\n{}",
                docker_platform_guidance(&platform_id(), &docker_status)
            ))
        } else {
            Err(detail)
        }
    }
}

fn docker_compose_check(project_dir: &Path) -> Option<String> {
    let compose_file = docker_compose_file(project_dir)?;
    let docker_status = check_docker();
    let mut problems = Vec::new();
    if !docker_status.cli_available {
        problems.push("Docker is not installed");
        problems.push("Docker Compose V2 has not been checked because Docker is unavailable");
    } else {
        if !docker_status.daemon_running {
            problems.push("Docker daemon is not running");
        }
        if !docker_status.compose_v2_available {
            problems.push("Docker Compose V2 is not installed");
        }
    }
    if problems.is_empty() {
        None
    } else {
        let platform = platform_id();
        Some(format!(
            "Project contains {}, but {}.\n\n{}",
            compose_file
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("docker-compose.yml"),
            problems.join("; "),
            docker_platform_guidance(&platform, &docker_status)
        ))
    }
}

fn docker_platform_guidance(platform: &str, status: &DockerStatus) -> String {
    let mut guidance = Vec::new();

    if !status.cli_available {
        guidance.push(match platform {
            "macos" => {
                "Docker (macOS): install OrbStack from https://orbstack.dev/download/stable/latest/arm64\nAfter installation, run `open -a OrbStack` and wait for OrbStack to finish starting."
            }
            "windows" => {
                "Docker (Windows): install Docker Desktop from https://docs.docker.com/desktop/install/windows-install/\nAfter installation, start Docker Desktop and wait until it reports that Docker is running."
            }
            "debian" => {
                "Docker (Ubuntu/Debian): install Docker Engine with the official guide at https://docs.docker.com/engine/install/\nThen run `sudo systemctl enable --now docker`."
            }
            "rhel" => {
                "Docker (CentOS/RHEL/Fedora): install Docker Engine with the official guide at https://docs.docker.com/engine/install/\nThen run `sudo systemctl enable --now docker`."
            }
            _ => {
                "Docker (Linux): install Docker Engine with the official guide at https://docs.docker.com/engine/install/\nThen start the Docker service using your distribution's service manager."
            }
        });
        guidance.push(
            "Docker Compose V2: install and verify this component after Docker is available. Run `docker compose version` to check it.",
        );
        return guidance.join("\n\n");
    }

    if !status.daemon_running {
        guidance.push(match platform {
            "macos" if status.provider == "orbstack" => {
                "Docker daemon (OrbStack): start OrbStack with `open -a OrbStack`, then retry. If needed, verify the active context with `docker context show`."
            }
            "macos" if status.provider == "colima" => {
                "Docker daemon (Colima): start it with `colima start`, then retry."
            }
            "macos" if status.provider == "rancher-desktop" => {
                "Docker daemon (Rancher Desktop): open Rancher Desktop and wait for its container engine to start, then retry."
            }
            "macos" if status.provider == "docker-desktop" => {
                "Docker daemon (Docker Desktop): start it with `open -a Docker`, then retry."
            }
            "macos" => {
                "Docker daemon: start the Docker runtime selected by `docker context show`, then retry."
            }
            "windows" => "Docker daemon: start Docker Desktop, then retry.",
            _ => "Docker daemon: start it with `sudo systemctl start docker`, then retry.",
        });
    }

    if !status.compose_v2_available {
        guidance.push(match platform {
            "macos" => {
                "Docker Compose V2: macOS 上 OrbStack/Docker Desktop 通常自带。若缺失，可通过下方命令手动下载官方二进制到 ~/.docker/cli-plugins/。"
            }
            "windows" => {
                "Docker Compose V2: Windows 上 Docker Desktop 通常自带。若缺失，可通过下方 PowerShell 命令手动下载官方二进制到 %USERPROFILE%\\.docker\\cli-plugins\\。"
            }
            "debian" => {
                "Docker Compose V2 (Ubuntu/Debian): 通过下方命令下载官方二进制到 ~/.docker/cli-plugins/ 即可。"
            }
            "rhel" => {
                "Docker Compose V2 (CentOS/RHEL/Fedora): 通过下方命令下载官方二进制到 ~/.docker/cli-plugins/ 即可。"
            }
            _ => {
                "Docker Compose V2 (Linux): 通过下方命令下载官方二进制到 ~/.docker/cli-plugins/ 即可。"
            }
        });
    }

    if guidance.is_empty() {
        "Docker and Docker Compose V2 are ready.".to_string()
    } else {
        guidance.join("\n\n")
    }
}

fn check_docker() -> DockerStatus {
    let cli_available = configured_command("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let mut compose_v2_available = cli_available
        && configured_command("docker")
            .args(["compose", "version", "--short"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    // Dev override: set AGENTSEEK_FORCE_NO_COMPOSE=1 to simulate a missing
    // Docker Compose v2 plugin for testing the confirmation page UI.
    if std::env::var("AGENTSEEK_FORCE_NO_COMPOSE").as_deref() == Ok("1") {
        compose_v2_available = false;
    }
    let daemon_running = cli_available
        && configured_command("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .map(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
                output.status.success()
                    && !stdout.trim().is_empty()
                    && !stderr.contains("cannot connect")
                    && !stderr.contains("permission denied")
                    && !stderr.contains("is the docker daemon running")
            })
            .unwrap_or(false);
    let provider = docker_provider(cli_available);
    DockerStatus {
        cli_available,
        compose_v2_available,
        daemon_running,
        provider,
    }
}

fn docker_provider(cli_available: bool) -> String {
    if !cli_available {
        return "unavailable".to_string();
    }
    let context = configured_command("docker")
        .args(["context", "show"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_lowercase())
        .unwrap_or_default();
    if context.contains("orbstack") {
        "orbstack".to_string()
    } else if context.contains("colima") {
        "colima".to_string()
    } else if context.contains("rancher") {
        "rancher-desktop".to_string()
    } else if context.contains("desktop") {
        "docker-desktop".to_string()
    } else if context.is_empty() || context == "default" {
        "docker-engine".to_string()
    } else {
        context
    }
}

fn instance_docker_status(project_dir: &Path) -> Option<DockerEnvironmentStatus> {
    let compose_file = docker_compose_file(project_dir)?;
    let docker_status = check_docker();
    let ready = docker_status.cli_available
        && docker_status.compose_v2_available
        && docker_status.daemon_running;
    let platform = platform_id();
    let (install_url, install_script, compose_install_command) = docker_install_resources(&platform);
    Some(DockerEnvironmentStatus {
        compose_file: compose_file.to_string_lossy().to_string(),
        platform: platform.clone(),
        provider: docker_status.provider.clone(),
        docker_installed: docker_status.cli_available,
        daemon_running: docker_status.daemon_running,
        compose_v2_installed: docker_status.compose_v2_available,
        ready,
        install_url,
        install_script,
        compose_install_command,
        start_supported: docker_status.cli_available && docker_start_supported(&platform, &docker_status.provider),
        guidance: (!ready).then(|| docker_platform_guidance(&platform, &docker_status)),
    })
}

fn docker_install_resources(platform: &str) -> (String, Option<String>, Option<String>) {
    match platform {
        "macos" => (
            "https://orbstack.dev/download/stable/latest/arm64".to_string(),
            None,
            Some(
                "mkdir -p ~/.docker/cli-plugins && curl -SL https://github.com/docker/compose/releases/latest/download/docker-compose-darwin-aarch64 -o ~/.docker/cli-plugins/docker-compose && chmod +x ~/.docker/cli-plugins/docker-compose".to_string(),
            ),
        ),
        "windows" => (
            "https://docs.docker.com/desktop/install/windows-install/".to_string(),
            None,
            Some(
                "New-Item -ItemType Directory -Force $env:USERPROFILE\\.docker\\cli-plugins; Invoke-WebRequest -Uri https://github.com/docker/compose/releases/latest/download/docker-compose-windows-x86_64.exe -OutFile $env:USERPROFILE\\.docker\\cli-plugins\\docker-compose.exe".to_string(),
            ),
        ),
        "debian" => (
            "https://docs.docker.com/engine/install/ubuntu/".to_string(),
            Some("curl -fsSL https://get.docker.com -o get-docker.sh && sh get-docker.sh".to_string()),
            Some(
                "ARCH=$(uname -m | sed 's/arm64/aarch64/') && mkdir -p ~/.docker/cli-plugins && curl -SL \"https://github.com/docker/compose/releases/latest/download/docker-compose-linux-${ARCH}\" -o ~/.docker/cli-plugins/docker-compose && chmod +x ~/.docker/cli-plugins/docker-compose".to_string(),
            ),
        ),
        "rhel" => (
            "https://docs.docker.com/engine/install/centos/".to_string(),
            Some("curl -fsSL https://get.docker.com -o get-docker.sh && sh get-docker.sh".to_string()),
            Some(
                "ARCH=$(uname -m | sed 's/arm64/aarch64/') && mkdir -p ~/.docker/cli-plugins && curl -SL \"https://github.com/docker/compose/releases/latest/download/docker-compose-linux-${ARCH}\" -o ~/.docker/cli-plugins/docker-compose && chmod +x ~/.docker/cli-plugins/docker-compose".to_string(),
            ),
        ),
        _ => (
            "https://docs.docker.com/engine/install/".to_string(),
            Some("curl -fsSL https://get.docker.com -o get-docker.sh && sh get-docker.sh".to_string()),
            Some(
                "ARCH=$(uname -m | sed 's/arm64/aarch64/') && mkdir -p ~/.docker/cli-plugins && curl -SL \"https://github.com/docker/compose/releases/latest/download/docker-compose-linux-${ARCH}\" -o ~/.docker/cli-plugins/docker-compose && chmod +x ~/.docker/cli-plugins/docker-compose".to_string(),
            ),
        ),
    }
}

fn docker_start_supported(platform: &str, provider: &str) -> bool {
    match platform {
        "macos" => matches!(provider, "orbstack" | "docker-desktop" | "colima" | "rancher-desktop"),
        "windows" | "debian" | "rhel" | "linux" => true,
        _ => false,
    }
}

fn start_docker_runtime(platform: &str, provider: &str) -> Result<(), String> {
    let mut command = match platform {
        "macos" if provider == "orbstack" => {
            let mut command = Command::new("/usr/bin/open");
            command.args(["-a", "OrbStack"]);
            command
        }
        "macos" if provider == "docker-desktop" => {
            let mut command = Command::new("/usr/bin/open");
            command.args(["-a", "Docker"]);
            command
        }
        "macos" if provider == "colima" => {
            let mut command = Command::new("colima");
            command.arg("start");
            command
        }
        "macos" if provider == "rancher-desktop" => {
            let mut command = Command::new("/usr/bin/open");
            command.args(["-a", "Rancher Desktop"]);
            command
        }
        "windows" => {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-Command",
                "Start-Process 'C:\\Program Files\\Docker\\Docker\\Docker Desktop.exe'",
            ]);
            command
        }
        "debian" | "rhel" | "linux" => {
            let mut command = Command::new("pkexec");
            command.args(["systemctl", "start", "docker"]);
            command
        }
        _ => return Err("Automatic Docker startup is not supported for the current runtime".to_string()),
    };
    let output = command
        .output()
        .map_err(|error| format!("Failed to start Docker runtime: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Err(if detail.trim().is_empty() {
            "Docker runtime startup failed".to_string()
        } else {
            detail.trim().to_string()
        })
    }
}

// ---------------------------------------------------------------------------
// Instance listing & lifecycle commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn list_instances(state: State<'_, DesktopState>) -> Result<Vec<InstanceRecord>, String> {
    let mut instances = state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?
        .instances
        .clone();
    // Re-calibrate from the on-disk lifecycle.toml / .env and persist any
    // drift so direct DB readers (e.g. the CLI) always see fresh values.
    let mut drift: Vec<InstanceRecord> = Vec::new();
    for instance in &mut instances {
        if enrich_service_endpoints(instance) {
            drift.push(instance.clone());
        }
    }
    if !drift.is_empty() && state.ensure_storage_ready().is_ok() {
        let mut failures: Vec<String> = Vec::new();
        let mut persisted: Vec<InstanceRecord> = Vec::new();
        // Persist first, then sync the in-memory store (storage → data lock
        // order, matching persist_instance / replace_vault_entries).
        if let Ok(mut storage) = state.storage.lock() {
            for instance in &drift {
                match storage.upsert_instance(instance) {
                    Ok(()) => persisted.push(instance.clone()),
                    Err(error) => failures.push(format!("{}: {error}", instance.id)),
                }
            }
        } else {
            failures.push("storage lock poisoned".to_string());
        }
        if !failures.is_empty() {
            state.log(
                None,
                "AgentSeek Desktop",
                "storage",
                "warn",
                format!(
                    "Failed to persist enriched instance(s): {}",
                    failures.join("; ")
                ),
                None,
            );
        }
        if !persisted.is_empty() {
            if let Ok(mut data) = state.data.lock() {
                for instance in persisted {
                    if let Some(slot) = data
                        .instances
                        .iter_mut()
                        .find(|item| item.id == instance.id)
                    {
                        *slot = instance;
                    }
                }
            }
        }
    }
    instances.sort_by_key(|instance| std::cmp::Reverse(instance.created_at));
    Ok(instances)
}

#[tauri::command]
async fn continue_install(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<InstanceRecord, String> {
    let state = state.inner().clone();
    state.set_deployment_stage(&instance_id, "tasks");
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<InstanceRecord, String> {
            let mut instance = instance_by_id(&state, &instance_id)?;
            if instance.env_path.is_none() {
                return Err("Please generate instance .env first".to_string());
            }
            ensure_docker_compose_ready(&state, &instance, "install")?;
            recheck_instance_ports(&state, &instance)?;
            instance.status = "installing".to_string();
            instance.updated_at = timestamp();
            enrich_service_endpoints(&mut instance);
            update_instance(&state, instance.clone())?;

            state.set_deployment_stage(&instance_id, "tasks");
            let tasks = run_and_log(&state, &instance, &["task", "--list"], "execution")?;
            // Parse every task name from `agentseek task --list` output
            // (format: "  task_name    description...") and execute each one.
            // Skip tasks that cannot run during the pre-start task phase:
            // - names starting with "ingest" import data and require the
            //   backend services to be fully running
            // - "relay-export" validates runtime ATOF events that only exist
            //   after live traffic has flowed through the Relay middleware
            // - names ending with "-stop" or "_stop" are teardown tasks
            //   (e.g. "phoenix-stop" runs docker compose down) that would
            //   undo earlier start tasks if executed in the same phase
            for line in tasks.output.lines() {
                let task_name = line.split_whitespace().next().unwrap_or("");
                if !task_name.is_empty()
                    && !task_name.starts_with("ingest")
                    && task_name != "relay-export"
                    && !task_name.ends_with("-stop")
                    && !task_name.ends_with("_stop")
                {
                    run_and_log(&state, &instance, &["task", task_name], "execution")?;
                }
            }
            let info = run_and_log(&state, &instance, &["info"], "execution")?;
            apply_info_urls(&mut instance, &info.output);
            enrich_service_endpoints(&mut instance);
            state.set_deployment_stage(&instance_id, "doctor");
            run_and_log(&state, &instance, &["doctor"], "execution")?;
            state.set_deployment_stage(&instance_id, "dry-run");
            run_and_log(&state, &instance, &["dev", "--dry-run"], "execution")?;
            state.set_deployment_stage(&instance_id, "starting");
            spawn_instance(&state, &mut instance)?;
            instance.status = "starting".to_string();
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            if let Err(error) = wait_for_instance_ready(&state, &instance) {
                let process_already_exited = instance
                    .pid
                    .is_some_and(|pid| !process_exists(pid));
                if !process_already_exited {
                    let _ = stop_instance_process(&state, &instance, "install");
                }
                return Err(error);
            }
            instance.status = "running".to_string();
            instance.needs_doctor = false;
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "success",
                "Instance deployment completed",
                None,
            );
            state.set_deployment_stage(&instance_id, "complete");
            Ok(instance)
        })();
        if let Err(error) = &result {
            state.set_deployment_stage(&instance_id, "failed");
            finalize_failed_deployment(&state, &instance_id, error, false);
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn deployment_progress(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<String, String> {
    Ok(state
        .deployment_stages
        .lock()
        .map_err(|_| "Deployment state lock is poisoned".to_string())?
        .get(&instance_id)
        .cloned()
        .unwrap_or_else(|| "pending".to_string()))
}

#[tauri::command]
async fn stop_instance(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<InstanceRecord, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut instance = instance_by_id(&state, &instance_id)?;
        enrich_service_endpoints(&mut instance);
        let previous_status = instance.status.clone();
        instance.status = "stopping".to_string();
        instance.updated_at = timestamp();
        update_instance(&state, instance.clone())?;
        let _stopped = match stop_instance_process(&state, &instance, "install") {
            Ok(stopped) => stopped,
            Err(error) => {
                instance.status = previous_status;
                instance.updated_at = timestamp();
                let _ = update_instance(&state, instance.clone());
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "error",
                    format!("Failed to stop instance: {error}"),
                    None,
                );
                return Err(error);
            }
        };
        instance.pid = None;
        instance.status = "stopped".to_string();
        instance.updated_at = timestamp();
        update_instance(&state, instance.clone())?;
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            "success",
            "Instance stopped",
            None,
        );
        Ok(instance)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn restart_instance(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<InstanceRecord, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = (|| -> Result<InstanceRecord, String> {
            let mut instance = instance_by_id(&state, &instance_id)?;
            ensure_docker_compose_ready(&state, &instance, "install")?;
            let synchronized = synchronize_instance_configs_from_env(&instance)?;
            if !synchronized.is_empty() {
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "config",
                    "info",
                    format!(
                        "Runtime configs updated based on instance .env before restart\n{}",
                        synchronized
                            .iter()
                            .map(|path| format!("  {}", path.display()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                    None,
                );
            }
            enrich_service_endpoints(&mut instance);
            instance.status = "restarting".to_string();
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            run_and_log(&state, &instance, &["doctor"], "execution")?;
            let _stopped = stop_instance_process(&state, &instance, "install")?;
            instance.pid = None;
            spawn_instance(&state, &mut instance)?;
            instance.status = "starting".to_string();
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            if let Err(error) = wait_for_instance_ready(&state, &instance) {
                let process_already_exited = instance
                    .pid
                    .is_some_and(|pid| !process_exists(pid));
                if !process_already_exited {
                    let _ = stop_instance_process(&state, &instance, "install");
                }
                return Err(error);
            }
            instance.status = "running".to_string();
            instance.needs_doctor = false;
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "success",
                "Doctor passed; instance restarted",
                None,
            );
            Ok(instance)
        })();
        if let Err(error) = &result {
            finalize_failed_deployment(&state, &instance_id, error, true);
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Shared failure cleanup for a deployment attempt: drop spooled runtime logs
/// and mark the instance failed (or needs-restart when a doctor check flagged it).
fn finalize_failed_deployment(
    state: &DesktopState,
    instance_id: &str,
    error: &str,
    needs_restart_on_doctor: bool,
) {
    remove_runtime_log_spool(state, instance_id);
    // Keep the runtime output so the failure can be diagnosed from the
    // Runtime tab. The lifecycle error below remains the concise summary.
    if let Ok(mut instance) = instance_by_id(state, instance_id) {
        instance.status = if needs_restart_on_doctor && instance.needs_doctor {
            "needs-restart".to_string()
        } else {
            "failed".to_string()
        };
        instance.updated_at = timestamp();
        let _ = update_instance(state, instance.clone());
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            "error",
            error.to_string(),
            None,
        );
    }
}

#[tauri::command]
fn mark_env_edited(state: State<'_, DesktopState>, instance_id: String) -> Result<(), String> {
    let mut instance = instance_by_id(&state, &instance_id)?;
    let deployment_completed = instance_has_completed_deployment(state.inner(), &instance)?;
    if !deployment_completed {
        instance.needs_doctor = false;
        instance.status = if instance.env_path.is_some() {
            "ready-to-install".to_string()
        } else {
            "configuring".to_string()
        };
        instance.updated_at = timestamp();
        update_instance(&state, instance.clone())?;
        state.log(
            Some(&instance.id),
            &instance.name,
            "lifecycle",
            "info",
            "Instance configuration changed; deployment is required",
            None,
        );
        return Ok(());
    }
    instance.needs_doctor = true;
    instance.status = "needs-restart".to_string();
    instance.updated_at = timestamp();
    update_instance(&state, instance.clone())?;
    state.log(
        Some(&instance.id),
        &instance.name,
        "lifecycle",
        "warning",
        "Instance configuration changed; restart is required",
        None,
    );
    Ok(())
}

#[tauri::command]
async fn delete_instance(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut instance = instance_by_id(&state, &instance_id)?;
        enrich_service_endpoints(&mut instance);
        instance.status = "deleting".to_string();
        instance.updated_at = timestamp();
        update_instance(&state, instance.clone())?;
        let result = (|| -> Result<(), String> {
            let stopped = if docker_compose_file(Path::new(&instance.work_dir)).is_some() {
                // Deletion uses `compose down` below; `compose stop` would fail
                // early when a damaged instance has already lost its .env.
                Vec::new()
            } else {
                stop_instance_process(&state, &instance, "install")
                    .map_err(|error| format!("Failed to stop instance associated processes: {error}"))?
            };
            instance.pid = None;
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
            // Clean up Docker containers (e.g. Phoenix, SeekDB) started by
            // lifecycle tasks or Compose services.
            // If cleanup fails, refuse to delete the working directory so
            // that the compose file remains available for manual cleanup.
            let docker_cleaned = match cleanup_docker_containers(&instance.work_dir) {
                Ok(cleaned) => {
                    if cleaned {
                        state.log(
                            Some(&instance.id),
                            &instance.name,
                            "install",
                            "info",
                            "Docker containers stopped and removed via docker compose down".to_string(),
                            None,
                        );
                    }
                    cleaned
                }
                Err(error) => {
                    state.log(
                        Some(&instance.id),
                        &instance.name,
                        "install",
                        "warning",
                        format!("Docker container cleanup failed: {error}"),
                        None,
                    );
                    return Err(format!(
                        "Failed to clean up Docker containers; instance working directory preserved for manual cleanup: {error}"
                    ));
                }
            };
            remove_instance_work_dir(&instance.work_dir)?;
            remove_runtime_log_spool(&state, &instance.id);
            state.remove_persisted_instance(&instance_id)?;
            {
                let mut data = state
                    .data
                    .lock()
                    .map_err(|_| "State lock is poisoned".to_string())?;
                data.instances.retain(|item| item.id != instance_id);
            }
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "success",
                format!(
                    "Instance deletion completed\nInstance name: {}\nInstance ID: {}\nWorking directory: {}\nProcesses stopped: {}\nDocker containers cleaned: {}\nInstance record: deleted",
                    instance.name,
                    instance.id,
                    instance.work_dir,
                    stopped.len(),
                    docker_cleaned
                ),
                None,
            );
            Ok(())
        })();
        if let Err(error) = &result {
            if let Ok(mut failed_instance) = instance_by_id(&state, &instance_id) {
                failed_instance.status = "delete-failed".to_string();
                failed_instance.updated_at = timestamp();
                let _ = update_instance(&state, failed_instance);
            }
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "error",
                format!("Failed to delete instance: {error}"),
                None,
            );
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn get_instance_docker_status(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<Option<DockerEnvironmentStatus>, String> {
    let instance = instance_by_id(&state, &instance_id)?;
    Ok(instance_docker_status(Path::new(&instance.work_dir)))
}

#[tauri::command]
fn start_instance_docker_runtime(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<(), String> {
    let instance = instance_by_id(&state, &instance_id)?;
    let status = instance_docker_status(Path::new(&instance.work_dir))
        .ok_or_else(|| "This instance does not require Docker Compose".to_string())?;
    if !status.docker_installed {
        return Err("Docker is not installed; automatic installation is not supported".to_string());
    }
    if status.daemon_running {
        return Ok(());
    }
    start_docker_runtime(&status.platform, &status.provider)
}

#[cfg(test)]
mod tests_instance {
    use super::*;

    #[test]
    fn docker_guidance_is_platform_specific() {
        let missing = DockerStatus {
            cli_available: false,
            compose_v2_available: false,
            daemon_running: false,
            provider: "unavailable".to_string(),
        };
        assert!(docker_platform_guidance("macos", &missing).contains("OrbStack"));
        assert!(docker_platform_guidance("windows", &missing).contains("Windows"));
        assert!(docker_platform_guidance("debian", &missing).contains("Ubuntu/Debian"));
        assert!(docker_platform_guidance("rhel", &missing).contains("CentOS/RHEL/Fedora"));
        let missing_compose = DockerStatus {
            cli_available: true,
            compose_v2_available: false,
            daemon_running: true,
            provider: "docker-engine".to_string(),
        };
        assert!(docker_platform_guidance("debian", &missing_compose).contains("cli-plugins"));
        assert!(docker_platform_guidance("rhel", &missing_compose).contains("cli-plugins"));
        let daemon_and_compose_missing = DockerStatus {
            cli_available: true,
            compose_v2_available: false,
            daemon_running: false,
            provider: "docker-engine".to_string(),
        };
        let guidance = docker_platform_guidance("debian", &daemon_and_compose_missing);
        assert!(guidance.contains("Docker daemon:"));
        assert!(guidance.contains("Docker Compose V2 (Ubuntu/Debian):"));
        let orbstack_stopped = DockerStatus {
            cli_available: true,
            compose_v2_available: true,
            daemon_running: false,
            provider: "orbstack".to_string(),
        };
        assert!(docker_platform_guidance("macos", &orbstack_stopped).contains("open -a OrbStack"));
    }

    #[test]
    fn replace_port_in_context_only_touches_port_like_contexts() {
        // URL contexts (`:port`) are replaced.
        assert_eq!(
            replace_port_in_context("http://127.0.0.1:6006/api", 6006, 64977),
            "http://127.0.0.1:64977/api"
        );
        // `*_PORT=` assignments are replaced; plain `foo=6006` is untouched.
        assert_eq!(
            replace_port_in_context("PHOENIX_PORT=6006\nSOME_KEY=6006", 6006, 64977),
            "PHOENIX_PORT=64977\nSOME_KEY=6006"
        );
        // Standalone `port` keyword (JSON, quotes, whitespace) is replaced.
        assert_eq!(
            replace_port_in_context("\"port\": 6006", 6006, 64977),
            "\"port\": 64977"
        );
        assert_eq!(
            replace_port_in_context("port 6006", 6006, 64977),
            "port 64977"
        );
        // Words ending in "port" (import/export/support) are NOT matched.
        assert_eq!(
            replace_port_in_context("import 6006", 6006, 64977),
            "import 6006"
        );
        assert_eq!(
            replace_port_in_context("# support 6006 items", 6006, 64977),
            "# support 6006 items"
        );
        // Numbers inside larger digit sequences and IP segments stay untouched.
        assert_eq!(
            replace_port_in_context("port 60067 and 192.168.6006.1", 6006, 64977),
            "port 60067 and 192.168.6006.1"
        );
        // JSON string values that merely look like the port are untouched.
        assert_eq!(
            replace_port_in_context("{\"other\": \"6006\"}", 6006, 64977),
            "{\"other\": \"6006\"}"
        );
    }

    #[test]
    fn lifecycle_services_expose_all_declared_urls() {
        let manifest: LifecycleManifest = toml::from_str(
            "[services.app]\nurl = \"http://127.0.0.1:5173\"\n[services.gateway]\nurl = \"http://127.0.0.1:8088/agent\"\n[services.copilotkit]\nurl = \"http://127.0.0.1:4000/api/copilotkit\"\n",
        )
        .expect("parse lifecycle services");

        assert_eq!(manifest.services.len(), 3);
        assert_eq!(service_display_name("app"), "Frontend");
        assert_eq!(service_display_name("gateway"), "Agent / Gateway");
        assert_eq!(service_display_name("copilotkit"), "CopilotKit Runtime");
    }
    #[test]
    fn deleting_an_instance_removes_its_working_directory() {
        let root = env::temp_dir().join(format!("agentseek-desktop-delete-{}", unique_stamp()));
        fs::create_dir_all(root.join("nested")).expect("create instance directory");
        fs::write(root.join("nested/data.txt"), "instance data").expect("write instance file");

        remove_instance_work_dir(&root.to_string_lossy()).expect("remove instance directory");

        assert!(!root.exists());
    }
    #[test]
    fn instance_working_directory_is_parent_plus_instance_name() {
        let parent = Path::new("/tmp/agentseek-instances");

        assert_eq!(
            instance_target_path(parent, "rag-development").expect("build target path"),
            parent.join("rag-development")
        );
        assert!(instance_target_path(parent, "nested/name").is_err());
        assert!(instance_target_path(parent, "..").is_err());
    }
    #[test]
    fn env_file_scan_prefers_example_and_ignores_nested_files() {
        let root = env::temp_dir().join(format!("agentseek-desktop-env-scan-{}", unique_stamp()));
        fs::create_dir_all(root.join("frontend")).expect("create test directory");
        for name in [
            ".env",
            ".env.example",
            ".env.development",
            ".env1",
            "README.md",
        ] {
            fs::write(root.join(name), "KEY=value\n").expect("write test file");
        }
        fs::write(root.join("frontend/.env"), "FRONTEND=true\n").expect("write nested env");

        let files = list_env_files(root.to_string_lossy().to_string()).expect("scan env files");

        assert_eq!(files.len(), 4);
        assert!(files[0].ends_with("/.env.example"));
        assert!(files.iter().any(|file| file.ends_with("/.env")));
        assert!(files.iter().any(|file| file.ends_with("/.env.development")));
        assert!(files.iter().any(|file| file.ends_with("/.env1")));
        assert!(files.iter().all(|file| !file.contains("frontend")));
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[cfg(unix)]
    #[test]
    fn terminate_processes_stops_parent_and_children() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn process tree");
        let pid = child.id();
        let waiter = std::thread::spawn(move || child.wait().expect("wait for process tree"));

        let stopped = super::terminate_processes([pid]).expect("terminate process tree");

        assert!(stopped.iter().any(|process| process.pid == pid));
        assert!(stopped.iter().all(|process| !process.executable.is_empty()));
        waiter.join().expect("join process waiter");
        assert!(!super::process_exists(pid));
    }
    #[test]
    fn apt_mirror_patch_adds_timeout_and_fallback() {
        let dir = patch_test_dir("apt-normal");
        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, "RUN apt-get update && apt-get install -y git\n").expect("write");
        patch_dockerfile_apt_mirror_if_needed(&dir);
        let patched = fs::read_to_string(&dockerfile).expect("read");
        assert!(patched.contains("timeout 60"));
        assert!(patched.contains("mirrors.aliyun.com"));
        assert!(!patched.contains("RUN apt-get update &&")); // original replaced
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn apt_mirror_patch_is_idempotent() {
        let dir = patch_test_dir("apt-idem");
        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, "RUN apt-get update && apt-get install -y git\n").expect("write");
        patch_dockerfile_apt_mirror_if_needed(&dir);
        let after_first = fs::read_to_string(&dockerfile).expect("read");
        patch_dockerfile_apt_mirror_if_needed(&dir);
        let after_second = fs::read_to_string(&dockerfile).expect("read");
        assert_eq!(after_first, after_second);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn apt_mirror_patch_skips_when_no_apt_get_update() {
        let dir = patch_test_dir("apt-none");
        let dockerfile = dir.join("Dockerfile");
        let original = "FROM python:3.12\nCMD [\"bash\"]\n";
        fs::write(&dockerfile, original).expect("write");
        patch_dockerfile_apt_mirror_if_needed(&dir);
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn apt_mirror_patch_skips_when_no_dockerfile() {
        let dir = patch_test_dir("apt-nofile");
        // Should not panic.
        patch_dockerfile_apt_mirror_if_needed(&dir);
        fs::remove_dir_all(&dir).ok();
    }
    fn sample_dockerfile_with_uv() -> String {
        [
            "FROM python:3.12-slim AS base",
            "WORKDIR /app",
            "RUN apt-get update && apt-get install -y git",
            "COPY pyproject.toml .",
            "RUN set -e; \\",
            "    if [ -n \"${UV_DEFAULT_INDEX:-}\" ]; then export UV_DEFAULT_INDEX; fi; \\",
            "    if [ -n \"${UV_INDEX_URL:-}\" ]; then export UV_INDEX_URL; fi; \\",
            "    if [ -n \"${UV_INSECURE_HOST:-}\" ]; then export UV_INSECURE_HOST; fi; \\",
            "    UV_LINK_MODE=copy uv sync --no-dev --no-cache",
            "",
        ]
        .join("\n")
    }
    #[test]
    fn mirrors_patch_adds_pypi_fallback() {
        let dir = patch_test_dir("mirrors-normal");
        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, sample_dockerfile_with_uv()).expect("write");
        patch_dockerfile_mirrors_if_needed(&dir);
        let patched = fs::read_to_string(&dockerfile).expect("read");
        assert!(patched.contains("mirrors.aliyun.com/pypi/simple/"));
        // Original fi; lines should be replaced with elif chain.
        assert!(patched.contains("elif [ -n \"${UV_INDEX_URL"));
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn mirrors_patch_is_idempotent() {
        let dir = patch_test_dir("mirrors-idem");
        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, sample_dockerfile_with_uv()).expect("write");
        patch_dockerfile_mirrors_if_needed(&dir);
        let after_first = fs::read_to_string(&dockerfile).expect("read");
        patch_dockerfile_mirrors_if_needed(&dir);
        let after_second = fs::read_to_string(&dockerfile).expect("read");
        assert_eq!(after_first, after_second);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn mirrors_patch_skips_when_no_uv_sync() {
        let dir = patch_test_dir("mirrors-nouv");
        let dockerfile = dir.join("Dockerfile");
        let original = "FROM python:3.12\nRUN pip install -r requirements.txt\n";
        fs::write(&dockerfile, original).expect("write");
        patch_dockerfile_mirrors_if_needed(&dir);
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn mirrors_patch_skips_when_no_default_index_marker() {
        let dir = patch_test_dir("mirrors-nomarker");
        let dockerfile = dir.join("Dockerfile");
        // Has uv sync but no UV_DEFAULT_INDEX block.
        let original = "FROM python:3.12\nRUN uv sync --no-dev\n";
        fs::write(&dockerfile, original).expect("write");
        patch_dockerfile_mirrors_if_needed(&dir);
        assert_eq!(fs::read_to_string(&dockerfile).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn cors_patch_replaces_port_specific_origins() {
        let dir = patch_test_dir("cors-normal");
        let path = dir.join("langgraph.json");
        let original = serde_json::json!({
            "graphs": { "agent": "./src/agent.py:graph" },
            "http": {
                "port": 8089,
                "cors": {
                    "allow_origins": ["http://127.0.0.1:5175"],
                    "allow_origin_regex": "^https?://.*:5175$"
                }
            }
        });
        fs::write(&path, original.to_string()).expect("write");
        patch_langgraph_cors_if_needed(&dir);
        let patched: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        let cors = patched["http"]["cors"].as_object().expect("cors object");
        assert_eq!(cors["allow_origin_regex"].as_str(), Some("^https?://.*$"));
        assert!(cors.get("allow_origins").is_none());
        assert_eq!(cors["allow_methods"], serde_json::json!(["*"]));
        assert_eq!(cors["allow_headers"], serde_json::json!(["*"]));
        // Non-CORS fields should be preserved.
        assert_eq!(patched["http"]["port"], 8089);
        assert_eq!(patched["graphs"]["agent"], "./src/agent.py:graph");
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn cors_patch_adds_trailing_newline() {
        let dir = patch_test_dir("cors-newline");
        let path = dir.join("langgraph.json");
        fs::write(&path, r#"{"http":{"cors":{"allow_origins":["http://localhost:5175"]}}}"#)
            .expect("write");
        patch_langgraph_cors_if_needed(&dir);
        let content = fs::read_to_string(&path).expect("read");
        assert!(content.ends_with('\n'), "patched file must end with a newline");
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn cors_patch_skips_when_no_cors_section() {
        let dir = patch_test_dir("cors-nocors");
        let path = dir.join("langgraph.json");
        let original = r#"{"graphs":{"agent":"./src/agent.py:graph"},"http":{"port":8089}}"#;
        fs::write(&path, original).expect("write");
        patch_langgraph_cors_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn cors_patch_skips_when_no_langgraph_json() {
        let dir = patch_test_dir("cors-nofile");
        // Should not panic.
        patch_langgraph_cors_if_needed(&dir);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn cors_patch_skips_invalid_json() {
        let dir = patch_test_dir("cors-invalid");
        let path = dir.join("langgraph.json");
        let original = "{not valid json}";
        fs::write(&path, original).expect("write");
        patch_langgraph_cors_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn convert_models_patch_replaces_optimum_command() {
        let dir = patch_test_dir("convert-normal");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("convert_models.py");
        fs::write(&path, "cmd = [sys.executable, \"-m\", \"optimum.exporters.openvino\", \"--model\", model_name]\n")
            .expect("write");
        patch_convert_models_if_needed(&dir);
        let patched = fs::read_to_string(&path).expect("read");
        assert!(patched.contains("\"optimum-cli\", \"export\", \"openvino\""));
        assert!(!patched.contains("\"-m\", \"optimum.exporters.openvino\""));
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn convert_models_patch_skips_when_already_patched() {
        let dir = patch_test_dir("convert-idem");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("convert_models.py");
        let original = "cmd = [\"optimum-cli\", \"export\", \"openvino\", \"--model\", model_name]\n";
        fs::write(&path, original).expect("write");
        patch_convert_models_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn convert_models_patch_skips_when_no_marker() {
        let dir = patch_test_dir("convert-nomarker");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("convert_models.py");
        let original = "print('hello')\n";
        fs::write(&path, original).expect("write");
        patch_convert_models_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn agent_async_patch_adds_shim() {
        let dir = patch_test_dir("agent-normal");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("agent.py");
        fs::write(&path, "from langchain_oceanbase.vectorstores import OceanbaseVectorStore\nprint('hi')\n")
            .expect("write");
        // pyproject.toml must list langchain-huggingface for the shim to be added.
        fs::write(dir.join("pyproject.toml"), "\"langchain-huggingface>=1.0\",\n").expect("write");
        patch_agent_async_if_needed(&dir);
        let patched = fs::read_to_string(&path).expect("read");
        assert!(patched.contains("_patched_agenerate"));
        assert!(patched.contains("_patched_astream"));
        assert!(patched.contains("asyncio.to_thread"));
        // Original marker should still be present.
        assert!(patched.contains("from langchain_oceanbase.vectorstores import OceanbaseVectorStore"));
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn agent_async_patch_is_idempotent() {
        let dir = patch_test_dir("agent-idem");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("agent.py");
        fs::write(&path, "from langchain_oceanbase.vectorstores import OceanbaseVectorStore\nprint('hi')\n")
            .expect("write");
        fs::write(dir.join("pyproject.toml"), "\"langchain-huggingface>=1.0\",\n").expect("write");
        patch_agent_async_if_needed(&dir);
        let after_first = fs::read_to_string(&path).expect("read");
        patch_agent_async_if_needed(&dir);
        let after_second = fs::read_to_string(&path).expect("read");
        assert_eq!(after_first, after_second);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn agent_async_patch_skips_when_no_hf_dep() {
        let dir = patch_test_dir("agent-nohf");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("agent.py");
        let original = "from langchain_core.prompts import ChatPromptTemplate\nprint('hi')\n";
        fs::write(&path, original).expect("write");
        // No langchain-huggingface in pyproject.toml — patch should skip.
        fs::write(dir.join("pyproject.toml"), "[project]\n").expect("write");
        patch_agent_async_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn agent_async_patch_skips_with_marker_but_no_hf_dep() {
        let dir = patch_test_dir("agent-markerno");
        let src = dir.join("src");
        fs::create_dir_all(&src).expect("mkdir");
        let path = src.join("agent.py");
        let original = "from langchain_oceanbase.vectorstores import OceanbaseVectorStore\nprint('hi')\n";
        fs::write(&path, original).expect("write");
        // Has OceanbaseVectorStore marker but no langchain-huggingface dep — should skip.
        fs::write(dir.join("pyproject.toml"), "[project]\n").expect("write");
        patch_agent_async_if_needed(&dir);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn agent_async_patch_skips_when_no_agent_py() {
        let dir = patch_test_dir("agent-nofile");
        // Should not panic.
        patch_agent_async_if_needed(&dir);
        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
/// Create a temp directory with a unique name.
fn patch_test_dir(label: &str) -> std::path::PathBuf {
    let dir = env::temp_dir().join(format!("agentseek-patch-{label}-{}", unique_stamp()));
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}
