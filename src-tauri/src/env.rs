// Environment variable parsing, rendering, and merging utilities.

fn is_secret_env_key(key: &str) -> bool {
    let normalized = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|marker| {
            normalized == *marker
                || normalized.starts_with(&format!("{marker}_"))
                || normalized.ends_with(&format!("_{marker}"))
                || normalized.contains(&format!("_{marker}_"))
        })
}

fn parse_env(content: &str) -> Vec<EnvVariable> {
    let mut entries = Vec::new();
    let mut comments = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            comments.push(trimmed.trim_start_matches('#').trim().to_string());
            continue;
        }
        if trimmed.is_empty() {
            if !comments.is_empty() {
                comments.push(String::new());
            }
            continue;
        }
        let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        if let Some((key, raw_value)) = assignment.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                let (value, inline_comment) = split_env_value(raw_value.trim());
                if let Some(inline_comment) = inline_comment {
                    comments.push(inline_comment);
                }
                entries.push(EnvVariable {
                    key: key.to_string(),
                    value,
                    comment: comments.join("\n").trim().to_string(),
                    source: "template".to_string(),
                    modified: false,
                });
                comments.clear();
            }
        }
    }
    entries
}

fn split_env_value(raw: &str) -> (String, Option<String>) {
    let mut quote = None;
    let mut escaped = false;
    let mut previous = None;
    for (index, character) in raw.char_indices() {
        if escaped {
            escaped = false;
            previous = Some(character);
            continue;
        }
        if character == '\\' && quote == Some('"') {
            escaped = true;
            previous = Some(character);
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
        } else if character == '#' && quote.is_none() && previous.is_some_and(char::is_whitespace) {
            let value = raw[..index].trim_end().to_string();
            let comment = raw[index + 1..].trim();
            return (value, (!comment.is_empty()).then(|| comment.to_string()));
        }
        previous = Some(character);
    }
    (raw.to_string(), None)
}

fn render_env(entries: &[EnvVariable]) -> String {
    let mut output = String::new();
    for entry in entries {
        if !entry.comment.trim().is_empty() {
            for line in entry.comment.lines() {
                output.push_str("# ");
                output.push_str(line.trim());
                output.push('\n');
            }
        }
        output.push_str(&entry.key);
        output.push('=');
        output.push_str(&entry.value);
        output.push_str("\n\n");
    }
    output
}

/// Sync `*_URL` and `*_ENDPOINT` env variables with ports resolved in
/// lifecycle.toml so that URLs like `LANGGRAPH_URL` and OTLP endpoints
/// like `AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` stay in sync
/// even when the `.env` file has no corresponding `*_PORT` variable.
fn sync_env_urls_from_lifecycle(work_dir: &str, entries: &mut [EnvVariable]) {
    let lifecycle_path = Path::new(work_dir).join(".agentseek/lifecycle.toml");
    let Ok(content) = fs::read_to_string(&lifecycle_path) else {
        return;
    };
    let Ok(manifest) = toml::from_str::<LifecycleManifest>(&content) else {
        return;
    };
    // Build a map of service-name-prefix -> port from lifecycle.toml service URLs.
    let lifecycle_ports: Vec<(String, u16)> = manifest
        .services
        .iter()
        .filter_map(|(name, service)| {
            extract_url_port(&service.url).map(|port| (name.to_ascii_uppercase(), port))
        })
        .collect();
    if lifecycle_ports.is_empty() {
        return;
    }
    let localhost_prefixes = LOOPBACK_URL_PREFIXES;
    for entry in entries.iter_mut() {
        let normalized_key = entry.key.to_ascii_uppercase();
        if !normalized_key.contains("URL") && !normalized_key.contains("ENDPOINT") {
            continue;
        }
        if !localhost_prefixes
            .iter()
            .any(|prefix| entry.value.starts_with(prefix))
        {
            continue;
        }
        if let Some((_, port)) = lifecycle_ports
            .iter()
            .filter(|(prefix, _)| normalized_key.contains(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
        {
            let updated = replace_url_port(&entry.value, *port);
            if updated != entry.value {
                entry.value = updated;
                entry.modified = true;
            }
            continue;
        }
        // For unmatched localhost ENDPOINT variables (e.g.
        // AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT), sync with the
        // Phoenix service port when present, otherwise fall back to the
        // first lifecycle port as a best-effort. This ensures OTLP
        // endpoints auto-follow PHOENIX_PORT even when the variable
        // name doesn't contain the service prefix.
        if normalized_key.contains("ENDPOINT") {
            let port = lifecycle_ports
                .iter()
                .find(|(prefix, _)| prefix == "PHOENIX")
                .or_else(|| lifecycle_ports.first())
                .map(|(_, port)| *port);
            if let Some(port) = port {
                let updated = replace_url_port(&entry.value, port);
                if updated != entry.value {
                    entry.value = updated;
                    entry.modified = true;
                }
            }
        }
    }
}

fn local_env_port_values(entries: &[EnvVariable]) -> Vec<(String, u16)> {
    entries
        .iter()
        .filter(|entry| is_local_service_port_key(&entry.key))
        .filter_map(|entry| {
            entry.value.trim().parse::<u16>().ok().map(|port| {
                (
                    entry
                        .key
                        .to_ascii_uppercase()
                        .trim_end_matches("_PORT")
                        .to_string(),
                    port,
                )
            })
        })
        .collect()
}

fn synchronize_env_entries(target: &mut [EnvVariable], root: &[EnvVariable]) {
    let root_by_key = root
        .iter()
        .map(|entry| (entry.key.to_ascii_uppercase(), entry))
        .collect::<HashMap<_, _>>();
    let local_ports = local_env_port_values(root);
    for entry in target.iter_mut() {
        let normalized_key = entry.key.to_ascii_uppercase();
        if let Some(source) = root_by_key.get(&normalized_key) {
            entry.value = source.value.clone();
            continue;
        }
        if !LOOPBACK_URL_PREFIXES
            .iter()
            .any(|prefix| entry.value.starts_with(prefix))
        {
            continue;
        }
        if let Some((_, port)) = local_ports
            .iter()
            .filter(|(prefix, _)| normalized_key.contains(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
        {
            entry.value = replace_url_port(&entry.value, *port);
        }
    }
}

fn merge_env_entries(source: &[EnvVariable], vault: &[EnvVariable]) -> Vec<EnvVariable> {
    let vault_by_key: HashMap<_, _> = vault
        .iter()
        .map(|entry| (entry.key.clone(), entry))
        .collect();
    source
        .iter()
        .cloned()
        .map(|mut entry| {
            if let Some(saved) = vault_by_key.get(&entry.key) {
                if entry.comment.is_empty() {
                    entry.comment = saved.comment.clone();
                }
                // Local service ports are instance-specific: vault values saved
                // by other instances must not override this instance's resolved
                // ports (e.g. a stale LANGGRAPH_PORT from an old instance would
                // otherwise break a freshly scaffolded one).
                //
                // URL/ENDPOINT values pointing at container-internal hosts
                // (e.g. http://phoenix:6006/v1/traces) are instance-specific
                // too: the template default is host-reachable (loopback) and a
                // stale vault value saved by another instance must not override
                // it for local-process instances. Remote hosts with a domain
                // name (user-customized collectors) still merge normally.
                let template_is_loopback = LOOPBACK_URL_PREFIXES
                    .iter()
                    .any(|prefix| entry.value.starts_with(prefix));
                let vault_is_container_host = (entry
                    .key
                    .to_ascii_uppercase()
                    .contains("URL")
                    || entry.key.to_ascii_uppercase().contains("ENDPOINT"))
                    && !LOOPBACK_URL_PREFIXES
                        .iter()
                        .any(|prefix| saved.value.starts_with(prefix))
                    && url_host(&saved.value)
                        .map(|host| !host.contains('.'))
                        .unwrap_or(false);
                if !is_local_service_port_key(&entry.key)
                    && !saved.value.trim().is_empty()
                    && !(template_is_loopback && vault_is_container_host)
                {
                    entry.value = saved.value.clone();
                    entry.source = "vault".to_string();
                }
            }
            entry
        })
        .collect()
}

fn merged_env(state: &DesktopState, source: &[EnvVariable]) -> Vec<EnvVariable> {
    let vault = state
        .data
        .lock()
        .map(|data| data.vault.clone())
        .unwrap_or_default();
    merge_env_entries(source, &vault)
}

/// Append vault entries whose key is absent from `entries`, so exporting a
/// vault never drops keys the source file does not contain.
fn append_vault_only_entries(entries: &mut Vec<EnvVariable>, vault: &[EnvVariable]) {
    for saved in vault {
        if saved.key.trim().is_empty() {
            continue;
        }
        if !entries.iter().any(|entry| entry.key == saved.key) {
            entries.push(saved.clone());
        }
    }
}

fn process_env_value(raw: &str) -> String {
    let value = raw.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if matches!(
            (bytes[0], bytes[value.len() - 1]),
            (b'\'', b'\'') | (b'"', b'"')
        ) {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn runtime_environment_summary(entries: &[EnvVariable]) -> String {
    entries
        .iter()
        .filter(|entry| {
            is_local_service_port_key(&entry.key)
                || (entry.key.to_ascii_uppercase().contains("URL")
                    && ["127.0.0.1", "localhost", "0.0.0.0", "[::1]"]
                        .iter()
                        .any(|host| entry.value.contains(host)))
        })
        .map(|entry| format!("{}={}", entry.key, entry.value))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Vault & instance env commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn list_vault(state: State<'_, DesktopState>) -> Result<Vec<EnvVariable>, String> {
    Ok(state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?
        .vault
        .clone())
}

#[tauri::command]
fn save_vault(state: State<'_, DesktopState>, entries: Vec<EnvVariable>) -> Result<(), String> {
    state.replace_vault_entries(entries)
}

#[tauri::command]
fn load_instance_env(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<Vec<EnvVariable>, String> {
    let instance = instance_by_id(&state, &instance_id)?;
    let path = instance
        .env_path
        .as_deref()
        .or(instance.env_example_path.as_deref())
        .ok_or_else(|| "Instance has no readable environment variable file".to_string())?;
    let content = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let parsed_env = parse_env(&content);
    Ok(merged_env(&state, &parsed_env))
}

/// Resolve lifecycle.toml ports and sync the resolved values back into the
/// .env entries (both `*_PORT` and `*_URL`/`*_ENDPOINT` variables).
/// Returns the list of port changes for logging.
fn resolve_and_sync_lifecycle_ports(
    instance: &InstanceRecord,
    lifecycle_path: &Path,
    state: &State<'_, DesktopState>,
    entries: &mut Vec<EnvVariable>,
) -> Result<Vec<PortChange>, String> {
    let reserved = collect_assigned_ports(state.inner(), Some(&instance.id));
    let (updated_lifecycle, changes, port_map) =
        resolve_lifecycle_ports(instance, &reserved, entries)?;
    // Write lifecycle.toml first, then update .env entries to match.
    fs::write(lifecycle_path, &updated_lifecycle)
        .map_err(|error| format!("Failed to write {}: {error}", lifecycle_path.display()))?;
    for (key, port) in &port_map {
        let new_value = port.to_string();
        if let Some(entry) = entries
            .iter_mut()
            .find(|e| e.key.to_ascii_uppercase() == *key)
        {
            if entry.value != new_value {
                entry.value = new_value;
                entry.modified = true;
            }
        } else {
            entries.push(EnvVariable {
                key: key.clone(),
                value: new_value,
                comment: format!(
                    "{} service port (auto-resolved)",
                    key.trim_end_matches("_PORT").to_ascii_lowercase()
                ),
                source: "instance".to_string(),
                modified: true,
            });
        }
    }
    sync_port_urls(entries, &port_map);
    // Sync unmatched localhost ENDPOINT variables by port: if the endpoint
    // still references a default port that was re-assigned due to a conflict,
    // update it to the resolved port. The `changes` mapping (old_port ->
    // new_port) is used instead of re-reading lifecycle.toml, which by this
    // point already contains the resolved ports.
    sync_endpoint_ports_by_changes(entries, &changes);
    Ok(changes)
}

/// Sync `*_URL`/`*_ENDPOINT` entries whose key contains a service prefix with
/// the resolved lifecycle port. Only loopback (host-reachable) URLs are
/// rewritten: container-internal service URLs (e.g.
/// RELAY_PHOENIX_ENDPOINT=http://phoenix:6006/v1/traces) reference the
/// in-network port and must keep it even when the host-facing port differs.
fn sync_port_urls(entries: &mut [EnvVariable], port_map: &[(String, u16)]) {
    for (key, port) in port_map {
        let prefix = key.trim_end_matches("_PORT");
        for entry in entries.iter_mut().filter(|e| {
            let k = e.key.to_ascii_uppercase();
            (k.contains("URL") || k.contains("ENDPOINT"))
                && k.contains(prefix)
                && LOOPBACK_URL_PREFIXES
                    .iter()
                    .any(|prefix| e.value.starts_with(prefix))
        }) {
            let updated = replace_url_port(&entry.value, *port);
            if updated != entry.value {
                entry.value = updated;
                entry.modified = true;
            }
        }
    }
}

/// Sync unmatched localhost ENDPOINT variables (e.g.
/// AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT) by port: if the endpoint
/// still references a port that was re-assigned due to a conflict, update it
/// to the resolved port. Only ports present in `changes.old_port` are touched,
/// so endpoints already pointing at resolved ports are left alone.
fn sync_endpoint_ports_by_changes(entries: &mut [EnvVariable], changes: &[PortChange]) {
    for entry in entries.iter_mut() {
        let normalized_key = entry.key.to_ascii_uppercase();
        if !normalized_key.contains("ENDPOINT") {
            continue;
        }
        if !LOCALHOST_URL_PREFIXES
            .iter()
            .any(|prefix| entry.value.starts_with(prefix))
        {
            continue;
        }
        if let Some(old_port) = extract_url_port(&entry.value) {
            if let Some(change) = changes.iter().find(|c| c.old_port == old_port) {
                let updated = replace_url_port(&entry.value, change.new_port);
                if updated != entry.value {
                    entry.value = updated;
                    entry.modified = true;
                }
            }
        }
    }
}

/// Extract the host portion of a URL value, without the port.
fn url_host(url: &str) -> Option<&str> {
    let remainder = url.split("://").nth(1)?;
    let authority = remainder.split(['/', '?', '#']).next()?;
    if authority.starts_with('[') {
        authority.split_once(']').map(|(host, _)| host)
    } else {
        authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .or(Some(authority))
    }
}

/// Restore non-loopback `*_URL`/`*_ENDPOINT` entries whose port drifted from
/// the template default in `.env.example`.
///
/// Container-internal endpoints (e.g. RELAY_PHOENIX_ENDPOINT) reference the
/// in-network port (phoenix:6006) and are never rewritten by port conflict
/// resolution, so a port mismatch with the example value means the value was
/// corrupted by an older buggy rewrite; align the port back to the default.
/// Entries whose host differs from the example (user-customized remote
/// endpoints) are left untouched.
fn restore_non_loopback_url_defaults(work_dir: &str, entries: &mut [EnvVariable]) {
    let example_path = Path::new(work_dir).join(".env.example");
    let Ok(example_content) = fs::read_to_string(&example_path) else {
        return;
    };
    let examples = parse_env(&example_content);
    for entry in entries.iter_mut() {
        let normalized_key = entry.key.to_ascii_uppercase();
        if !normalized_key.contains("URL") && !normalized_key.contains("ENDPOINT") {
            continue;
        }
        if LOOPBACK_URL_PREFIXES
            .iter()
            .any(|prefix| entry.value.starts_with(prefix))
        {
            continue;
        }
        let Some(example) = examples
            .iter()
            .find(|e| e.key.eq_ignore_ascii_case(&entry.key))
        else {
            continue;
        };
        // The template default is host-reachable (loopback) but the current
        // value references a container-internal host (e.g. phoenix:6006).
        // For local-process instances that host does not resolve on the
        // host machine, so the value was corrupted; restore the template
        // default and let the later lifecycle sync align its port.
        if LOOPBACK_URL_PREFIXES
            .iter()
            .any(|prefix| example.value.starts_with(prefix))
            && !LOOPBACK_URL_PREFIXES
                .iter()
                .any(|prefix| entry.value.starts_with(prefix))
        {
            entry.value = example.value.clone();
            entry.modified = true;
            continue;
        }
        let (Some(current_port), Some(default_port)) = (
            extract_url_port(&entry.value),
            extract_url_port(&example.value),
        ) else {
            continue;
        };
        if current_port == default_port {
            continue;
        }
        if url_host(&entry.value) != url_host(&example.value) {
            continue;
        }
        let updated = replace_url_port(&entry.value, default_port);
        if updated != entry.value {
            entry.value = updated;
            entry.modified = true;
        }
    }
}

#[tauri::command]
fn save_instance_env(
    state: State<'_, DesktopState>,
    input: SaveEnvInput,
) -> Result<SaveEnvResult, String> {
    state.ensure_storage_ready()?;
    let mut instance = instance_by_id(&state, &input.instance_id)?;
    let deployment_completed = instance_has_completed_deployment(state.inner(), &instance)?;
    let env_path = PathBuf::from(&instance.work_dir).join(".env");
    if env_path.is_file() && !input.overwrite {
        return Err(format!("ENV_FILE_EXISTS:{}", env_path.display()));
    }
    let mut entries = input.entries;
    // Drop rows whose key is empty (e.g. a row added on the client and left
    // blank); otherwise render_env would write a broken "=value" line.
    entries.retain(|entry| !entry.key.trim().is_empty());
    let lifecycle_path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    let port_changes = if deployment_completed || !lifecycle_path.is_file() {
        if !deployment_completed {
            resolve_port_conflicts(&mut entries)?
        } else {
            // A deployed instance's own services legitimately occupy their
            // ports, so host-availability is only a conflict for ports outside
            // its own set; duplicates within the .env and ports reserved by
            // other instances are always treated as conflicts.
            let reserved = collect_assigned_ports(state.inner(), Some(&instance.id));
            let self_ports = instance_self_ports(&instance);
            resolve_deployed_port_conflicts(&mut entries, &reserved, &self_ports)?
        }
    } else {
        resolve_and_sync_lifecycle_ports(&instance, &lifecycle_path, &state, &mut entries)?
    };
    // Ensure LangSmith tracing is disabled by default to prevent 403 Forbidden
    // warnings from langgraph_api.metadata when no LANGCHAIN_API_KEY is configured.
    // Respect user's explicit LANGSMITH_TRACING setting if present in .env.
    if !entries
        .iter()
        .any(|e| e.key.eq_ignore_ascii_case("LANGSMITH_TRACING"))
    {
        entries.push(EnvVariable {
            key: "LANGSMITH_TRACING".to_string(),
            value: "false".to_string(),
            comment: "Disable LangSmith tracing to avoid metadata submission warnings"
                .to_string(),
            source: "instance".to_string(),
            modified: true,
        });
    }
    fs::write(&env_path, render_env(&entries)).map_err(|error| error.to_string())?;
    let root = PathBuf::from(&instance.work_dir);
    let mut synchronized = synchronize_instance_project_name(&root, &instance.name)?
        .into_iter()
        .collect::<Vec<_>>();
    for path in synchronize_instance_port_configs(&root, &entries)? {
        if !synchronized.contains(&path) {
            synchronized.push(path);
        }
    }

    let mut synced_count = 0;
    {
        let mut data = state
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?;
        for entry in entries.iter().filter(|entry| entry.modified) {
            // Local service ports are instance-specific runtime config, not
            // shared credentials: keeping them out of the global vault prevents
            // one instance's resolved ports from leaking into newly scaffolded
            // instances (e.g. stale LANGGRAPH_PORT overriding a fresh render).
            if is_local_service_port_key(&entry.key) {
                continue;
            }
            synced_count += 1;
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
    instance.env_path = Some(env_path.to_string_lossy().to_string());
    if deployment_completed {
        instance.status = "needs-restart".to_string();
        instance.needs_doctor = true;
    } else {
        instance.status = "ready-to-install".to_string();
        instance.needs_doctor = false;
    }
    instance.updated_at = timestamp();
    enrich_service_endpoints(&mut instance);
    state.persist_current_vault()?;
    update_instance(&state, instance.clone())?;
    if !port_changes.is_empty() {
        let details = port_change_details(&port_changes);
        state.log(
            Some(&instance.id),
            &instance.name,
            "config",
            "warning",
            format!(
                "Local port conflicts detected; free ports auto-assigned and synced to instance runtime configs and env vault\nPort changes:\n{details}\nSynced files:\n  {}\n{}",
                env_path.display(),
                synchronized
                    .iter()
                    .map(|path| format!("  {}", path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            None,
        );
    }
    let docker_warning = docker_compose_check(&root);
    if let Some(message) = &docker_warning {
        state.log(
            Some(&instance.id),
            &instance.name,
            "config",
            "error",
            message,
            Some("docker --version && docker compose version --short && docker info".to_string()),
        );
    }
    state.log(
        Some(&instance.id),
        &instance.name,
        "config",
        "success",
        format!(
            "Generated {} ({} keys, synced {} to vault)",
            env_path.display(),
            entries.len(),
            synced_count
        ),
        None,
    );
    let saved_entries: Vec<EnvVariable> = entries
        .into_iter()
        .map(|mut entry| {
            entry.modified = false;
            entry
        })
        .collect();
    Ok(SaveEnvResult {
        path: env_path.to_string_lossy().to_string(),
        key_count: saved_entries.len(),
        synced_count,
        port_changes,
        entries: saved_entries,
        docker_warning,
    })
}

// ---------------------------------------------------------------------------
// Env import / export commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn import_env(state: State<'_, DesktopState>, path: String) -> Result<usize, String> {
    state.ensure_storage_ready()?;
    let file = PathBuf::from(path.trim());
    if !file.is_file()
        || !file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_env_file_name)
    {
        return Err("Please select a valid .env file path".to_string());
    }
    let entries = parse_env(&fs::read_to_string(&file).map_err(|error| error.to_string())?);
    let count = entries.len();
    {
        let mut data = state
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?;
        for mut entry in entries {
            entry.source = "import".to_string();
            entry.modified = false;
            if let Some(saved) = data.vault.iter_mut().find(|saved| saved.key == entry.key) {
                *saved = entry;
            } else {
                data.vault.push(entry);
            }
        }
    }
    state.persist_current_vault()?;
    state.log(
        None,
        "Config Center",
        "config",
        "success",
        format!("Imported {count} variables from {}", file.display()),
        None,
    );
    Ok(count)
}

fn is_env_file_name(name: &str) -> bool {
    name.starts_with(".env")
}

#[tauri::command]
fn list_env_files(path: String) -> Result<Vec<String>, String> {
    let directory = PathBuf::from(path.trim());
    if !directory.is_dir() {
        return Err("Please select an existing project directory".to_string());
    }
    let directory = fs::canonicalize(&directory).map_err(|error| error.to_string())?;
    let mut files = fs::read_dir(&directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_env_file_name)
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        let left_name = left
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let right_name = right
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        (left_name != ".env.example", left_name).cmp(&(right_name != ".env.example", right_name))
    });
    Ok(files
        .into_iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect())
}

#[tauri::command]
fn export_env(
    state: State<'_, DesktopState>,
    input: ExportEnvInput,
) -> Result<ExportEnvResult, String> {
    let source = PathBuf::from(input.source_path.trim());
    if !source.is_file()
        || !source
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_env_file_name)
    {
        return Err("Please select a valid source .env file".to_string());
    }
    let output = PathBuf::from(input.output_path.trim());
    if !output.is_absolute()
        || output
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| name.trim().is_empty())
    {
        return Err("Target file must be an absolute path with filename".to_string());
    }
    if !output.parent().is_some_and(Path::is_dir) {
        return Err("Target file directory does not exist".to_string());
    }
    let source_entries =
        parse_env(&fs::read_to_string(&source).map_err(|error| error.to_string())?);
    let vault = state
        .data
        .lock()
        .map(|data| data.vault.clone())
        .unwrap_or_default();
    let mut entries = merge_env_entries(&source_entries, &vault);
    // Exports carry the whole vault, not just keys the source file already
    // contains: a sparse or empty source file must not yield an empty export.
    append_vault_only_entries(&mut entries, &vault);
    if output.is_file() && !input.overwrite {
        return Err(format!("ENV_FILE_EXISTS:{}", output.display()));
    }
    fs::write(&output, render_env(&entries)).map_err(|error| error.to_string())?;
    let filled_count = entries
        .iter()
        .filter(|entry| entry.source == "vault" && !entry.value.trim().is_empty())
        .count();
    let missing_count = entries
        .iter()
        .filter(|entry| entry.value.trim().is_empty())
        .count();
    state.log(
        None,
        "Config Center",
        "config",
        "success",
        format!(
            "Exported {}\nTotal variables: {}\nBackfilled from vault: {}\nStill missing: {}\nSource: {}",
            output.display(),
            entries.len(),
            filled_count,
            missing_count,
            source.display()
        ),
        None,
    );
    Ok(ExportEnvResult {
        path: output.to_string_lossy().to_string(),
        key_count: entries.len(),
        filled_count,
        missing_count,
    })
}

#[cfg(test)]
mod tests_env {
    use super::*;

    #[test]
    fn sync_port_urls_rewrites_only_loopback_urls() {
        let mut entries = vec![
            EnvVariable {
                key: "RELAY_PHOENIX_ENDPOINT".to_string(),
                value: "http://phoenix:6006/v1/traces".to_string(),
                ..EnvVariable::default()
            },
            EnvVariable {
                key: "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT".to_string(),
                value: "http://phoenix:6006/v1/traces".to_string(),
                ..EnvVariable::default()
            },
            EnvVariable {
                key: "PHOENIX_URL".to_string(),
                value: "http://127.0.0.1:6006".to_string(),
                ..EnvVariable::default()
            },
            EnvVariable {
                key: "PHOENIX_PORT".to_string(),
                value: "53750".to_string(),
                ..EnvVariable::default()
            },
        ];
        sync_port_urls(&mut entries, &[("PHOENIX_PORT".to_string(), 53750)]);
        // Container-internal endpoints keep their in-network port 6006.
        assert_eq!(entries[0].value, "http://phoenix:6006/v1/traces");
        assert_eq!(entries[1].value, "http://phoenix:6006/v1/traces");
        // Host-reachable loopback URLs follow the resolved port.
        assert_eq!(entries[2].value, "http://127.0.0.1:53750");
        // *_PORT entries are left to the dedicated port-map loop.
        assert_eq!(entries[3].value, "53750");
    }
    #[test]
    fn env_round_trip_preserves_comments_and_values() {
        let input = "# API endpoint\nOPENAI_BASE_URL=https://example.com/v1\n\n# Secret\nOPENAI_API_KEY=test-key\n";
        let entries = parse_env(input);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].comment, "API endpoint");
        assert_eq!(entries[1].value, "test-key");

        let rendered = render_env(&entries);
        assert!(rendered.contains("# API endpoint\nOPENAI_BASE_URL=https://example.com/v1"));
        assert!(rendered.contains("# Secret\nOPENAI_API_KEY=test-key"));
    }
    #[test]
    fn empty_vault_values_do_not_hide_template_defaults() {
        let source = parse_env("MODEL=openai:gpt-4o-mini\nPORT=5173\nAPI_KEY=\n");
        let vault = vec![
            EnvVariable {
                key: "MODEL".to_string(),
                value: String::new(),
                comment: "Model name".to_string(),
                source: "instance".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "PORT".to_string(),
                value: "6000".to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
        ];

        let merged = merge_env_entries(&source, &vault);
        assert_eq!(merged[0].value, "openai:gpt-4o-mini");
        assert_eq!(merged[0].source, "template");
        assert_eq!(merged[0].comment, "Model name");
        // Local service ports are instance-specific: vault values must not
        // override the instance's resolved port.
        assert_eq!(merged[1].value, "5173");
        assert_eq!(merged[1].source, "template");
        assert!(merged[2].value.is_empty());
    }
    #[test]
    fn non_port_variables_are_still_overridden_by_vault() {
        let source = parse_env("MODEL=openai:gpt-4o-mini\nAPI_KEY=\n");
        let vault = vec![EnvVariable {
            key: "MODEL".to_string(),
            value: "openai:gpt-4o".to_string(),
            comment: "Model name".to_string(),
            source: "instance".to_string(),
            modified: false,
        }];

        let merged = merge_env_entries(&source, &vault);
        assert_eq!(merged[0].value, "openai:gpt-4o");
        assert_eq!(merged[0].source, "vault");
        assert_eq!(merged[0].comment, "Model name");
    }
    #[test]
    fn vault_only_entries_are_appended_when_exporting() {
        let source = parse_env("EXISTING=source-value\n");
        let vault = vec![
            EnvVariable {
                key: "EXISTING".to_string(),
                value: "vault-value".to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "VAULT_ONLY".to_string(),
                value: "secret".to_string(),
                comment: "From vault".to_string(),
                source: "instance".to_string(),
                modified: false,
            },
        ];

        let mut entries = merge_env_entries(&source, &vault);
        append_vault_only_entries(&mut entries, &vault);
        // Existing key backfilled from vault, vault-only key appended exactly once.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].value, "vault-value");
        assert_eq!(entries[1].key, "VAULT_ONLY");
        assert_eq!(entries[1].value, "secret");
        assert_eq!(entries[1].comment, "From vault");
    }
    #[test]
    fn exporting_an_empty_source_yields_the_whole_vault() {
        // Regression: a sparse/empty source file used to produce a zero-key
        // export because vault-only entries were never appended.
        let source = parse_env("");
        let vault = vec![
            EnvVariable {
                key: "API_KEY".to_string(),
                value: "k1".to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "MODEL".to_string(),
                value: "gpt-4o".to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "".to_string(),
                value: "orphan".to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
        ];

        let mut entries = merge_env_entries(&source, &vault);
        append_vault_only_entries(&mut entries, &vault);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "API_KEY");
        assert_eq!(entries[1].key, "MODEL");
    }
    #[test]
    fn inline_env_comments_are_parsed_without_breaking_hash_values() {
        let entries = parse_env("MODEL=openai:gpt-4o # Default model\nTOKEN=abc#123\n");
        assert_eq!(entries[0].value, "openai:gpt-4o");
        assert_eq!(entries[0].comment, "Default model");
        assert_eq!(entries[1].value, "abc#123");
        assert_eq!(
            split_env_value("\"value # text\" # note").1.as_deref(),
            Some("note")
        );
    }
    #[test]
    fn parse_env_empty_input_returns_empty_vec() {
        let entries = parse_env("");
        assert!(entries.is_empty());
    }
    #[test]
    fn parse_env_whitespace_only_lines_returns_empty_vec() {
        let entries = parse_env("   \n\t\n\n  \t  \n");
        assert!(entries.is_empty());
    }
    #[test]
    fn parse_env_value_with_hash_is_preserved() {
        let entries = parse_env("KEY=value#with#hash");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "KEY");
        // Hash inside a value should be preserved (not treated as comment)
        assert_eq!(entries[0].value, "value#with#hash");
    }
    #[test]
    fn parse_env_duplicate_keys_keep_last() {
        let entries = parse_env("KEY=val1\nKEY=val2\nKEY=val3");
        // parse_env does not deduplicate; all entries are returned
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[2].value, "val3");
    }
    #[test]
    fn parse_env_very_long_value_is_preserved() {
        let long_value = "x".repeat(10_000);
        let input = format!("KEY={long_value}");
        let entries = parse_env(&input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].value, long_value);
    }
    #[test]
    fn render_env_round_trips_basic_entries() {
        let original = "KEY1=value1\nKEY2=value2\n";
        let entries = parse_env(original);
        let rendered = render_env(&entries);
        assert!(rendered.contains("KEY1=value1"));
        assert!(rendered.contains("KEY2=value2"));
    }
    #[test]
    fn sync_env_urls_syncs_localhost_endpoint_without_service_prefix() {
        // AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT should auto-follow
        // the Phoenix port from lifecycle.toml even though the variable
        // name doesn't contain "PHOENIX" or "URL".
        let root = env::temp_dir().join(format!(
            "agentseek-desktop-endpoint-sync-{}",
            unique_stamp()
        ));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        fs::write(
            metadata.join("lifecycle.toml"),
            "version = 1\n[services.phoenix]\nurl = \"http://127.0.0.1:64977\"\n",
        )
        .expect("write lifecycle");
        let mut entries = parse_env(
            "PHOENIX_PORT=64977\n\
             AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:6006/v1/traces\n",
        );
        sync_env_urls_from_lifecycle(&root.to_string_lossy(), &mut entries);
        let endpoint = entries
            .iter()
            .find(|e| e.key == "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(
            endpoint.value, "http://127.0.0.1:64977/v1/traces",
            "OTLP endpoint should follow Phoenix lifecycle port"
        );
        assert!(endpoint.modified, "endpoint should be marked as modified");
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn endpoint_ports_follow_resolved_port_changes_only() {
        // An OTLP endpoint referencing the re-assigned default phoenix port
        // must follow the resolved port.
        let changes = [
            PortChange {
                key: "PHOENIX_PORT".to_string(),
                old_port: 6006,
                new_port: 64977,
            },
            PortChange {
                key: "FRONTEND_PORT".to_string(),
                old_port: 5173,
                new_port: 5175,
            },
        ];
        let mut entries = parse_env(
            "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:6006/v1/traces\n\
             PHOENIX_URL=http://127.0.0.1:64977\n\
             VITE_API_URL=http://127.0.0.1:5175\n",
        );
        sync_endpoint_ports_by_changes(&mut entries, &changes);
        let endpoint = entries
            .iter()
            .find(|e| e.key == "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(
            endpoint.value, "http://127.0.0.1:64977/v1/traces",
            "endpoint referencing the re-assigned default port should follow it"
        );
        assert!(endpoint.modified, "endpoint should be marked as modified");

        // Endpoints/URLs already pointing at resolved ports are left alone,
        // even when the port also appears as another service's new port.
        let phoenix_url = entries
            .iter()
            .find(|e| e.key == "PHOENIX_URL")
            .expect("url entry should exist");
        let vite_url = entries
            .iter()
            .find(|e| e.key == "VITE_API_URL")
            .expect("url entry should exist");
        assert_eq!(phoenix_url.value, "http://127.0.0.1:64977");
        assert!(!phoenix_url.modified, "already-resolved URL must not be rewritten");
        assert_eq!(vite_url.value, "http://127.0.0.1:5175");
        assert!(!vite_url.modified, "already-resolved URL must not be rewritten");

        // Re-running with the same changes is idempotent: no port in the
        // values matches any old_port anymore.
        sync_endpoint_ports_by_changes(&mut entries, &changes);
        let endpoint = entries
            .iter()
            .find(|e| e.key == "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(endpoint.value, "http://127.0.0.1:64977/v1/traces");
    }
    #[test]
    fn sync_env_urls_otlp_endpoint_follows_phoenix_not_first_service() {
        // When lifecycle.toml lists multiple services with backend first,
        // the OTLP endpoint must still follow the PHOENIX service port
        // rather than the first (backend) port.
        let root = env::temp_dir().join(format!(
            "agentseek-desktop-endpoint-phoenix-{}",
            unique_stamp()
        ));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        fs::write(
            metadata.join("lifecycle.toml"),
            "version = 1\n\
             [services.backend]\n\
             url = \"http://127.0.0.1:59302\"\n\
             [services.frontend]\n\
             url = \"http://127.0.0.1:5175\"\n\
             [services.phoenix]\n\
             url = \"http://127.0.0.1:59297\"\n",
        )
        .expect("write lifecycle");
        let mut entries = parse_env(
            "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:6006/v1/traces\n",
        );
        sync_env_urls_from_lifecycle(&root.to_string_lossy(), &mut entries);
        let endpoint = entries
            .iter()
            .find(|e| e.key == "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(
            endpoint.value, "http://127.0.0.1:59297/v1/traces",
            "OTLP endpoint should follow the PHOENIX service port"
        );
        assert!(endpoint.modified, "endpoint should be marked as modified");
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn sync_env_urls_preserves_non_localhost_endpoints() {
        // Remote URLs in ENDPOINT variables should not be modified.
        let root = env::temp_dir().join(format!(
            "agentseek-desktop-endpoint-remote-{}",
            unique_stamp()
        ));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        fs::write(
            metadata.join("lifecycle.toml"),
            "version = 1\n[services.phoenix]\nurl = \"http://127.0.0.1:64977\"\n",
        )
        .expect("write lifecycle");
        let mut entries = parse_env(
            "OTEL_EXPORTER_ENDPOINT=https://remote-phoenix.example.com:6006/v1/traces\n",
        );
        sync_env_urls_from_lifecycle(&root.to_string_lossy(), &mut entries);
        let endpoint = entries
            .iter()
            .find(|e| e.key == "OTEL_EXPORTER_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(
            endpoint.value,
            "https://remote-phoenix.example.com:6006/v1/traces",
            "remote endpoints should not be modified"
        );
        assert!(!endpoint.modified, "remote endpoint should not be modified");
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn restore_non_loopback_url_defaults_fixes_polluted_container_endpoints() {
        // A container-internal endpoint corrupted by an older rewrite (e.g.
        // phoenix:60663) must be restored to the .env.example default (6006),
        // while already-correct non-loopback endpoints stay untouched.
        let root = env::temp_dir().join(format!(
            "agentseek-desktop-restore-defaults-{}",
            unique_stamp()
        ));
        fs::create_dir_all(&root).expect("create root directory");
        fs::write(
            root.join(".env.example"),
            "RELAY_PHOENIX_ENDPOINT=http://phoenix:6006/v1/traces\n\
             OTEL_EXPORTER_ENDPOINT=https://collector.example.com:4318/v1/traces\n",
        )
        .expect("write example env");
        let mut entries = parse_env(
            "RELAY_PHOENIX_ENDPOINT=http://phoenix:60663/v1/traces\n\
             OTEL_EXPORTER_ENDPOINT=https://collector.example.com:4318/v1/traces\n",
        );
        restore_non_loopback_url_defaults(&root.to_string_lossy(), &mut entries);
        let endpoint = entries
            .iter()
            .find(|e| e.key == "RELAY_PHOENIX_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(
            endpoint.value, "http://phoenix:6006/v1/traces",
            "polluted container-internal endpoint should be restored to the template default"
        );
        assert!(endpoint.modified, "restored endpoint should be marked as modified");
        let remote = entries
            .iter()
            .find(|e| e.key == "OTEL_EXPORTER_ENDPOINT")
            .expect("remote endpoint should exist");
        assert_eq!(remote.value, "https://collector.example.com:4318/v1/traces");
        assert!(!remote.modified, "matching remote endpoint must not be touched");
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn restore_non_loopback_url_defaults_keeps_customized_remote_hosts() {
        // A user-pointed remote host (different from the example) must never
        // be restored, even when the port happens to match the example.
        let root = env::temp_dir().join(format!(
            "agentseek-desktop-restore-custom-{}",
            unique_stamp()
        ));
        fs::create_dir_all(&root).expect("create root directory");
        fs::write(
            root.join(".env.example"),
            "OTEL_EXPORTER_ENDPOINT=https://collector.example.com:4318/v1/traces\n",
        )
        .expect("write example env");
        let mut entries = parse_env(
            "OTEL_EXPORTER_ENDPOINT=https://custom-collector.example.com:4318/v1/traces\n",
        );
        restore_non_loopback_url_defaults(&root.to_string_lossy(), &mut entries);
        let remote = entries
            .iter()
            .find(|e| e.key == "OTEL_EXPORTER_ENDPOINT")
            .expect("remote endpoint should exist");
        assert_eq!(
            remote.value, "https://custom-collector.example.com:4318/v1/traces",
            "user-customized remote host must not be restored"
        );
        assert!(!remote.modified);
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn restore_non_loopback_url_defaults_leaves_loopback_entries_alone() {
        // Loopback entries are owned by port conflict resolution; the restore
        // pass must not revert resolved ports back to template defaults.
        let root = env::temp_dir().join(format!(
            "agentseek-desktop-restore-loopback-{}",
            unique_stamp()
        ));
        fs::create_dir_all(&root).expect("create root directory");
        fs::write(root.join(".env.example"), "PHOENIX_URL=http://127.0.0.1:6006\n")
            .expect("write example env");
        let mut entries = parse_env("PHOENIX_URL=http://127.0.0.1:63320\n");
        restore_non_loopback_url_defaults(&root.to_string_lossy(), &mut entries);
        let url = entries
            .iter()
            .find(|e| e.key == "PHOENIX_URL")
            .expect("url entry should exist");
        assert_eq!(url.value, "http://127.0.0.1:63320");
        assert!(!url.modified, "loopback entries are handled by conflict resolution");
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn restore_non_loopback_url_defaults_restores_container_host_to_loopback_default() {
        // A local-process instance whose OTLP endpoint was rewritten to the
        // container host (http://phoenix:6006/v1/traces) while the template
        // default is loopback must be restored to the template value; the
        // later lifecycle sync aligns the port.
        let root = env::temp_dir().join(format!(
            "agentseek-desktop-restore-container-host-{}",
            unique_stamp()
        ));
        fs::create_dir_all(&root).expect("create root directory");
        fs::write(
            root.join(".env.example"),
            "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:6006/v1/traces\n",
        )
        .expect("write example env");
        let mut entries = parse_env(
            "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://phoenix:6006/v1/traces\n",
        );
        restore_non_loopback_url_defaults(&root.to_string_lossy(), &mut entries);
        let endpoint = entries
            .iter()
            .find(|e| e.key == "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(
            endpoint.value, "http://127.0.0.1:6006/v1/traces",
            "container-host endpoint must be restored to the loopback template default"
        );
        assert!(endpoint.modified);
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn merge_env_entries_ignores_stale_container_host_vault_values() {
        // Template default is loopback; the shared vault holds a stale
        // container-internal value (http://phoenix:6006/v1/traces) saved by an
        // older instance. The vault value must not override the template
        // default for the freshly scaffolded instance.
        let source = parse_env(
            "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:6006/v1/traces\n\
             PHOENIX_PORT=56438\n\
             AGENTSEEK_MODEL=openai:glm-4.5\n",
        );
        let vault = parse_env(
            "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://phoenix:6006/v1/traces\n\
             AGENTSEEK_MODEL=openai:glm-5.2\n",
        );
        let merged = merge_env_entries(&source, &vault);
        let endpoint = merged
            .iter()
            .find(|e| e.key == "AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(
            endpoint.value, "http://127.0.0.1:6006/v1/traces",
            "stale container-host vault value must not override the loopback template default"
        );
        assert_ne!(endpoint.source, "vault");
        // Non-URL vault values still merge normally.
        let model = merged
            .iter()
            .find(|e| e.key == "AGENTSEEK_MODEL")
            .expect("model entry should exist");
        assert_eq!(model.value, "openai:glm-5.2");
        assert_eq!(model.source, "vault");
    }
    #[test]
    fn merge_env_entries_keeps_user_customized_remote_endpoints() {
        // A remote collector host with a domain name is user-customized and
        // must still merge over the loopback template default.
        let source = parse_env("OTEL_EXPORTER_ENDPOINT=http://127.0.0.1:6006/v1/traces\n");
        let vault = parse_env(
            "OTEL_EXPORTER_ENDPOINT=https://collector.example.com:4318/v1/traces\n",
        );
        let merged = merge_env_entries(&source, &vault);
        let endpoint = merged
            .iter()
            .find(|e| e.key == "OTEL_EXPORTER_ENDPOINT")
            .expect("endpoint entry should exist");
        assert_eq!(
            endpoint.value, "https://collector.example.com:4318/v1/traces",
            "user-customized remote endpoint must be kept"
        );
        assert_eq!(endpoint.source, "vault");
    }
}
