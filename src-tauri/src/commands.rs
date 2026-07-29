// Tauri command handlers: CLI status, runtime install, instance lifecycle,
// vault, env, logs, storage, and system info.

// ---------------------------------------------------------------------------
// CLI status
// ---------------------------------------------------------------------------

#[tauri::command]
async fn cli_status(check_latest: Option<bool>) -> Result<CliStatus, String> {
    tauri::async_runtime::spawn_blocking(move || current_cli_status(check_latest.unwrap_or(true)))
        .await
        .map_err(|error| error.to_string())?
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

// ---------------------------------------------------------------------------
// Template / instance / vault listing
// ---------------------------------------------------------------------------

#[tauri::command]
async fn list_templates(state: State<'_, DesktopState>) -> Result<Vec<TemplateInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        // Refresh template cache from GitHub before listing.
        refresh_template_cache();
        let result = run_cli(&["create", "--list-templates"], None)?;
        if result.code != 0 {
            return Err(result.output);
        }
        let templates = parse_templates(&result.output);
        let cli_path = agentseek_program();
        state.log(
            None,
            "AgentSeek Desktop",
            "lifecycle",
            "info",
            format!(
                "agentseek CLI: {}\nagentseek version: {}\nuv tool dir: {}\n--list-templates returned {} templates\n{}",
                cli_path,
                agentseek_command_version(&cli_path).unwrap_or_default(),
                uv_tool_bin_dir(),
                templates.len(),
                result.output
            ),
            None,
        );
        Ok(templates)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn list_instances(state: State<'_, DesktopState>) -> Result<Vec<InstanceRecord>, String> {
    let mut instances = state
        .data
        .lock()
        .map_err(|_| "State lock is poisoned".to_string())?
        .instances
        .clone();
    for instance in &mut instances {
        enrich_service_endpoints(instance);
    }
    instances.sort_by_key(|instance| std::cmp::Reverse(instance.created_at));
    Ok(instances)
}

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

// ---------------------------------------------------------------------------
// Instance preparation & env management
// ---------------------------------------------------------------------------

#[tauri::command]
async fn prepare_instance(
    state: State<'_, DesktopState>,
    input: PrepareInstanceInput,
) -> Result<PrepareInstanceResult, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let first_run = *state
            .storage_setup_required
            .lock()
            .map_err(|_| "Storage setup state lock is poisoned".to_string())?;
        if !first_run {
            state.ensure_storage_ready()?;
        }
        if input.name.trim().is_empty() {
            return Err("Instance name cannot be empty".to_string());
        }
        {
            let data = state
                .data
                .lock()
                .map_err(|_| "State lock is poisoned".to_string())?;
            if data
                .instances
                .iter()
                .any(|instance| instance.name == input.name.trim())
            {
                return Err("Instance name already exists".to_string());
            }
        }

        let parent = PathBuf::from(input.target_dir.trim());
        if input.target_dir.trim().is_empty() {
            return Err("Instance working directory cannot be empty".to_string());
        }
        fs::create_dir_all(&parent).map_err(|error| format!("Failed to create instance working directory: {error}"))?;
        if !parent.is_dir() {
            return Err("Instance working path is not a directory".to_string());
        }
        let target = instance_target_path(&parent, &input.name)?;
        validate_target(&target)?;
        let staging = parent.join(format!(".agentseek-desktop-{}", unique_stamp()));
        fs::create_dir_all(&staging).map_err(|error| error.to_string())?;

        let instance_id = format!("instance-{}", unique_stamp());
        state.log(
            Some(&instance_id),
            &input.name,
            "install",
            "info",
            format!("Starting instance creation\nInstance working directory: {}", target.display()),
            None,
        );
        // Describe template to extract port defaults before creation.
        let describe_result = run_cli(&["create", "--describe", &input.template_id], None)
            .map_err(|error| format!("Failed to read template description: {error}"))?;
        if describe_result.code != 0 {
            return Err(format!("Failed to read template description: {}", describe_result.output));
        }
        let reserved = collect_assigned_ports(&state, None);
        let (mut resolved_ports, mut port_changes) =
            resolve_describe_ports(&describe_result.output, &reserved)?;

        let create_started = Instant::now();
        let result = match run_cli(
            &["create", &input.template_id, "--no-input"],
            Some(&staging),
        ) {
            Ok(result) => result,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        state.log(
            Some(&instance_id),
            &input.name,
            "install",
            if result.code == 0 { "success" } else { "error" },
            &result.output,
            Some(result.command.clone()),
        );
        if result.code != 0 {
            let _ = fs::remove_dir_all(&staging);
            return Err(result.output);
        }
        state.log(
            Some(&instance_id),
            &input.name,
            "install",
            "success",
            format!(
                "AgentSeek create completed in {} seconds",
                create_started.elapsed().as_secs()
            ),
            None,
        );

        let generated = match fs::read_dir(&staging)
            .map_err(|error| error.to_string())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| path.is_dir())
            }) {
            Ok(Some(generated)) => generated,
            Ok(None) => {
                let _ = fs::remove_dir_all(&staging);
                return Err("AgentSeek CLI did not return generated project directory".to_string());
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        if target.exists() {
            fs::remove_dir(&target).map_err(|error| error.to_string())?;
        }
        fs::rename(&generated, &target).map_err(|error| format!("Failed to move instance directory: {error}"))?;
        let _ = fs::remove_dir_all(&staging);
        // Propagate the instance name into generated files that still hold the template default.
        if let Some(project_name) = &describe_result
            .output
            .lines()
            .find_map(|line| line.trim().strip_prefix("project_name:"))
            .map(|value| value.trim().to_string())
        {
            if project_name != input.name.trim() {
                let _ =
                    replace_project_name_in_directory(&target, &project_name, input.name.trim());
            }
        }
        synchronize_instance_project_name(&target, input.name.trim())?;
        // Patch convert_models.py to use `optimum-cli export openvino` instead of
        // `python -m optimum.exporters.openvino` (which has no __main__ entry point
        // and silently does nothing). Only affects agentic-rag-openvino instances.
        patch_convert_models_if_needed(&target);
        // Patch agent.py to add an async compatibility shim for
        // HuggingFacePipeline (ChatHuggingFace._astream doesn't check
        // for HuggingFacePipeline and crashes on async_client access).
        patch_agent_async_if_needed(&target);
        // Patch Dockerfile to fall back to mirrors.aliyun.com when the
        // official Debian apt sources are unreachable (common in China).
        patch_dockerfile_apt_mirror_if_needed(&target);
        // Patch Dockerfile to add GitHub + PyPI mirror fallbacks for slow
        // connections in China. Tests actual download speed from the
        // pyproject.toml GitHub dependencies and falls back to ghfast.top /
        // mirrors.aliyun.com when direct access is too slow.
        patch_dockerfile_mirrors_if_needed(&target);
        // Patch langgraph.json CORS to allow any origin (templates hardcode a
        // specific frontend port that doesn't match the dynamically assigned one).
        patch_langgraph_cors_if_needed(&target);

        let env_example =
            find_env_example(&target).ok_or_else(|| ".env.example not found in instance".to_string())?;
        let env_content = fs::read_to_string(&env_example).map_err(|error| error.to_string())?;
        let mut parsed_env = parse_env(&env_content);

        // Resolve service ports from lifecycle.toml — some services (e.g. backend:2024)
        // may not appear in cookiecutter variables and thus were missed by resolve_describe_ports.
        // Also handle services that share a port with a cookiecutter variable (e.g. service "app"
        // whose URL uses {{ cookiecutter.frontend_port }}) — these should NOT create a duplicate
        // APP_PORT entry but follow the resolved port of the shared variable.
        let lifecycle_path = target.join(".agentseek/lifecycle.toml");
        if let Ok(lifecycle_content) = fs::read_to_string(&lifecycle_path) {
            if let Ok(manifest) = toml::from_str::<LifecycleManifest>(&lifecycle_content) {
                let mut taken: HashSet<u16> = reserved.iter().copied().collect();
                for (_, port) in &resolved_ports {
                    taken.insert(*port);
                }
                let mut updated = lifecycle_content.clone();
                for (name, service) in &manifest.services {
                    let default_port = extract_url_port(&service.url).unwrap_or(0);
                    if default_port == 0 {
                        continue;
                    }
                    let env_key = format!("{}_PORT", name.to_ascii_uppercase());

                    // 1. Already resolved under this key (from describe output)
                    if let Some(&p) = resolved_ports.get(&env_key) {
                        if p != default_port {
                            let new_url = replace_url_port(&service.url, p);
                            updated = updated.replace(&service.url, &new_url);
                        }
                        continue;
                    }

                    // 2. Port shared with another variable that was changed — apply same change
                    if let Some(change) = port_changes.iter().find(|c| c.old_port == default_port) {
                        if change.new_port != default_port {
                            let new_url = replace_url_port(&service.url, change.new_port);
                            updated = updated.replace(&service.url, &new_url);
                        }
                        continue; // Don't create duplicate entry
                    }

                    // 3. Port shared with another variable (unchanged) — skip
                    if taken.contains(&default_port) {
                        continue; // Don't create duplicate entry
                    }

                    // 4. Service port not in describe output and not shared — resolve now
                    let port = if port_is_available(default_port) {
                        taken.insert(default_port);
                        default_port
                    } else {
                        let mut replacement = available_ephemeral_port()?;
                        while taken.contains(&replacement) {
                            replacement = available_ephemeral_port()?;
                        }
                        taken.insert(replacement);
                        port_changes.push(PortChange {
                            key: env_key.clone(),
                            old_port: default_port,
                            new_port: replacement,
                        });
                        replacement
                    };
                    resolved_ports.insert(env_key.clone(), port);
                    if port != default_port {
                        let new_url = replace_url_port(&service.url, port);
                        updated = updated.replace(&service.url, &new_url);
                    }
                }
                if updated != lifecycle_content {
                    fs::write(&lifecycle_path, &updated).map_err(|error| {
                        format!("Failed to write {}: {error}", lifecycle_path.display())
                    })?;
                }
            }
        }

        // Add resolved port env variables from describe output.
        // New port variables (not in .env.example) are marked modified=true to sync to vault;
        // Existing port variables are marked modified only when value changes.
        let existing_keys: HashSet<String> = parsed_env
            .iter()
            .map(|e| e.key.to_ascii_uppercase())
            .collect();
        for (key, port) in &resolved_ports {
            let new_value = port.to_string();
            if !existing_keys.contains(key.as_str()) {
                // Check if this port value is already covered by another entry
                // (e.g. GATEWAY_PORT=8088 and BUB_AG_UI_PORT=8088 are the same)
                let covered = parsed_env.iter().any(|e| {
                    e.key.to_ascii_uppercase() != *key && e.value.trim() == new_value
                });
                if covered {
                    continue; // Do not create duplicate entry
                }
                // Check if port changed from default; if so, sync existing entries
                // (e.g. 8088 occupied -> assigned 58781; need to update BUB_AG_UI_PORT and URL)
                if let Some(change) = port_changes.iter().find(|c| c.key == *key) {
                    let old_value = change.old_port.to_string();
                    for entry in parsed_env.iter_mut() {
                        if entry.value.trim() == old_value {
                            // Port value match — update directly (e.g. BUB_AG_UI_PORT)
                            entry.value = new_value.clone();
                            entry.modified = true;
                        } else if extract_url_port(&entry.value) == Some(change.old_port) {
                            // URL contains old port — sync update (e.g. BUB_AG_UI_AGENT_URL)
                            let updated_url = replace_url_port(&entry.value, *port);
                            if updated_url != entry.value {
                                entry.value = updated_url;
                                entry.modified = true;
                            }
                        }
                    }
                    continue; // Do not create GATEWAY_PORT; already updated BUB_AG_UI_PORT
                }
                parsed_env.push(EnvVariable {
                    key: key.clone(),
                    value: new_value,
                    comment: String::new(),
                    source: "describe".to_string(),
                    modified: true,
                });
            } else if let Some(entry) = parsed_env
                .iter_mut()
                .find(|e| e.key.to_ascii_uppercase() == *key)
            {
                if entry.value != new_value {
                    entry.value = new_value;
                    entry.modified = true;
                }
            }
        }

        // Ensure LangSmith tracing is disabled by default to prevent 403 Forbidden
        // warnings from langgraph_api.metadata when no LANGCHAIN_API_KEY is configured.
        // Respect user's explicit LANGSMITH_TRACING setting if present in .env.
        if !parsed_env
            .iter()
            .any(|e| e.key.to_ascii_uppercase() == "LANGSMITH_TRACING")
        {
            parsed_env.push(EnvVariable {
                key: "LANGSMITH_TRACING".to_string(),
                value: "false".to_string(),
                comment: "Disable LangSmith tracing to avoid metadata submission warnings"
                    .to_string(),
                source: "instance".to_string(),
                modified: true,
            });
        }
        let env = merged_env(&state, &parsed_env);
        let now = timestamp();
        let mut instance = InstanceRecord {
            id: instance_id,
            name: input.name.trim().to_string(),
            template_id: input.template_id,
            status: "configuring".to_string(),
            deployment_mode: "local".to_string(),
            work_dir: target.to_string_lossy().to_string(),
            env_example_path: Some(env_example.to_string_lossy().to_string()),
            env_path: None,
            note: input.note,
            created_at: now,
            updated_at: now,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: Some(input.name.trim().to_string()),
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };
        enrich_service_endpoints(&mut instance);
        state.persist_instance(&instance)?;
        state
            .data
            .lock()
            .map_err(|_| "State lock is poisoned".to_string())?
            .instances
            .push(instance.clone());
        if let Some(message) = docker_compose_check(&target) {
            state.log(
                Some(&instance.id),
                &instance.name,
                "install",
                "error",
                &message,
                Some("docker --version && docker compose version --short && docker info".to_string()),
            );
            instance.status = "failed".to_string();
            instance.updated_at = timestamp();
            let _ = update_instance(&state, instance.clone());
            return Err(format!("{} instance startup process exited, please check lifecycle logs", instance.name));
        }
        Ok(PrepareInstanceResult {
            instance,
            env,
            docker_warning: None,
        })
    })
    .await
    .map_err(|error| error.to_string())?
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
    let lifecycle_path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    let port_changes = if deployment_completed || !lifecycle_path.is_file() {
        if !deployment_completed {
            resolve_port_conflicts(&mut entries)?
        } else {
            Vec::new()
        }
    } else {
        let reserved = collect_assigned_ports(state.inner(), Some(&instance.id));
        let (updated_lifecycle, changes, port_map) = resolve_lifecycle_ports(&instance, &reserved, &entries)?;
        // Write lifecycle.toml first, then update .env entries to match.
        fs::write(&lifecycle_path, &updated_lifecycle)
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
                // .env missing this port variable; create new entry and write to .env + vault
                entries.push(EnvVariable {
                    key: key.clone(),
                    value: new_value,
                    comment: format!("{} service port (auto-resolved)", key.trim_end_matches("_PORT").to_ascii_lowercase()),
                    source: "instance".to_string(),
                    modified: true,
                });
            }
        }
        // Sync *_URL variables with the resolved lifecycle ports so that
        // URLs like LANGGRAPH_URL stay in sync even when no *_PORT variable
        // exists in the .env file.
        for (key, port) in &port_map {
            let prefix = key.trim_end_matches("_PORT");
            for entry in entries.iter_mut().filter(|e| {
                let k = e.key.to_ascii_uppercase();
                k.contains("URL") && k.contains(prefix)
            }) {
                let updated = replace_url_port(&entry.value, *port);
                if updated != entry.value {
                    entry.value = updated;
                    entry.modified = true;
                }
            }
        }
        changes
    };
    // Ensure LangSmith tracing is disabled by default to prevent 403 Forbidden
    // warnings from langgraph_api.metadata when no LANGCHAIN_API_KEY is configured.
    // Respect user's explicit LANGSMITH_TRACING setting if present in .env.
    if !entries
        .iter()
        .any(|e| e.key.to_ascii_uppercase() == "LANGSMITH_TRACING")
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
// Instance deployment lifecycle
// ---------------------------------------------------------------------------

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
            // Skip tasks whose name starts with "ingest" — they import data
            // and require the backend services to be fully running, which is
            // not guaranteed during the pre-start task phase.
            for line in tasks.output.lines() {
                let task_name = line.trim().split_whitespace().next().unwrap_or("");
                if !task_name.is_empty() && !task_name.starts_with("ingest") {
                    run_and_log(&state, &instance, &["task", task_name], "execution")?;
                }
            }
            let info = run_and_log(&state, &instance, &["info"], "execution")?;
            apply_info_urls(&mut instance, &info.output);
            enrich_service_endpoints(&mut instance);
            state.set_deployment_stage(&instance_id, "doctor");
            run_and_log(&state, &instance, &["doctor"], "execution")?;
            ensure_docker_compose_ready(&state, &instance, "install")?;
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
            remove_runtime_log_spool(&state, &instance_id);
            // Instance failed to run; clean up runtime log (error info already shown in lifecycle log)
            if let Ok(mut storage) = state.storage.lock() {
                let _ = storage.delete_runtime_logs(&instance_id);
            }
            if let Ok(mut data) = state.data.lock() {
                data.logs.retain(|log| {
                    !(log.instance_id.as_deref() == Some(instance_id.as_str())
                        && log.category == "runtime")
                });
            }
            if let Ok(mut instance) = instance_by_id(&state, &instance_id) {
                instance.status = "failed".to_string();
                instance.updated_at = timestamp();
                let _ = update_instance(&state, instance.clone());
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "error",
                    error.clone(),
                    None,
                );
            }
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

// ---------------------------------------------------------------------------
// Instance stop / restart / delete
// ---------------------------------------------------------------------------

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
            ensure_docker_compose_ready(&state, &instance, "install")?;
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
            remove_runtime_log_spool(&state, &instance_id);
            // Instance failed to run; clean up runtime log (error info already shown in lifecycle log)
            if let Ok(mut storage) = state.storage.lock() {
                let _ = storage.delete_runtime_logs(&instance_id);
            }
            if let Ok(mut data) = state.data.lock() {
                data.logs.retain(|log| {
                    !(log.instance_id.as_deref() == Some(instance_id.as_str())
                        && log.category == "runtime")
                });
            }
            if let Ok(mut instance) = instance_by_id(&state, &instance_id) {
                instance.status = if instance.needs_doctor {
                    "needs-restart".to_string()
                } else {
                    "failed".to_string()
                };
                instance.updated_at = timestamp();
                let _ = update_instance(&state, instance.clone());
                state.log(
                    Some(&instance.id),
                    &instance.name,
                    "install",
                    "error",
                    error.clone(),
                    None,
                );
            }
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
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
        return update_instance(&state, instance);
    }
    instance.needs_doctor = true;
    instance.status = "needs-restart".to_string();
    instance.updated_at = timestamp();
    update_instance(&state, instance)
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
            let stopped = stop_instance_process(&state, &instance, "install")
                .map_err(|error| format!("Failed to stop instance associated processes: {error}"))?;
            instance.pid = None;
            instance.updated_at = timestamp();
            update_instance(&state, instance.clone())?;
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
                    "Instance deletion completed\nInstance name: {}\nInstance ID: {}\nWorking directory: {}\nProcesses stopped: {}\nInstance record: deleted",
                    instance.name,
                    instance.id,
                    instance.work_dir,
                    stopped.len()
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

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

#[tauri::command]
fn list_logs(state: State<'_, DesktopState>, query: LogQuery) -> Result<LogPage, String> {
    sync_runtime_log_spools(state.inner());
    state
        .storage
        .lock()
        .map_err(|_| "Storage lock is poisoned".to_string())?
        .query_logs(&query)
}

#[tauri::command]
fn log_settings(state: State<'_, DesktopState>) -> Result<LogSettings, String> {
    let retention_days = state
        .storage_config
        .lock()
        .map_err(|_| "Storage config lock is poisoned".to_string())?
        .runtime_log_retention_days;
    Ok(LogSettings {
        runtime_retention_days: retention_days,
    })
}

#[tauri::command]
fn save_log_settings(
    state: State<'_, DesktopState>,
    settings: LogSettings,
) -> Result<LogSettings, String> {
    state.ensure_storage_ready()?;
    if !(1..=3_650).contains(&settings.runtime_retention_days) {
        return Err("Runtime log retention days must be between 1 and 3650".to_string());
    }
    let mut config = state
        .storage_config
        .lock()
        .map_err(|_| "Storage config lock is poisoned".to_string())?
        .clone();
    config.runtime_log_retention_days = settings.runtime_retention_days;
    write_storage_config(&state.config_path, &config)?;
    *state
        .storage_config
        .lock()
        .map_err(|_| "Storage config lock is poisoned".to_string())? = config;
    let removed = state
        .storage
        .lock()
        .map_err(|_| "Storage lock is poisoned".to_string())?
        .cleanup_logs(settings.runtime_retention_days, timestamp())?;
    state.log(
        None,
        "Log Center",
        "config",
        "success",
        format!(
            "Runtime log retention set to {} days; cleaned up {} log entries",
            settings.runtime_retention_days, removed
        ),
        None,
    );
    Ok(settings)
}

// ---------------------------------------------------------------------------
// Env import / export
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
    let entries = merged_env(&state, &source_entries);
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

// ---------------------------------------------------------------------------
// Storage configuration
// ---------------------------------------------------------------------------

fn ensure_seekdb_runtime(data_dir: &Path) -> Result<PathBuf, String> {
    let runtime = data_dir.join("runtime/seekdb-python");
    let python = if cfg!(windows) {
        runtime.join("Scripts/python.exe")
    } else {
        runtime.join("bin/python")
    };
    if !python.is_file() {
        let uv = uv_program().ok_or_else(|| "Please install uv before configuring SeekDB".to_string())?;
        run_dependency_command(
            &uv,
            &["venv", &runtime.to_string_lossy(), "--python", "3.12"],
            "Creating AgentSeek Desktop SeekDB private Python environment",
        )?;
    }
    let marker = runtime.join(".pyseekdb-installed");
    if !marker.is_file() {
        let uv = uv_program().ok_or_else(|| "Please install uv before configuring SeekDB".to_string())?;
        run_dependency_command(
            &uv,
            &[
                "pip",
                "install",
                "--python",
                &python.to_string_lossy(),
                "--upgrade",
                "pyseekdb",
            ],
            "Installing AgentSeek Desktop private pyseekdb",
        )?;
        fs::write(&marker, "pyseekdb").map_err(|error| error.to_string())?;
    }
    Ok(python)
}

#[tauri::command]
fn storage_status(state: State<'_, DesktopState>) -> Result<StorageStatus, String> {
    storage_status_value(state.inner())
}

fn storage_status_value(state: &DesktopState) -> Result<StorageStatus, String> {
    let config = state
        .storage_config
        .lock()
        .map_err(|_| "Storage config lock is poisoned".to_string())?
        .clone();
    let error = state
        .storage_error
        .lock()
        .ok()
        .and_then(|error| error.clone());
    Ok(StorageStatus {
        mode: config.mode,
        effective_mode: state
            .effective_storage_mode
            .lock()
            .map_err(|_| "Storage state lock is poisoned".to_string())?
            .clone(),
        path: config.path,
        default_sqlite_path: state.data_dir.to_string_lossy().to_string(),
        default_seekdb_path: state.data_dir.join("seekdb").to_string_lossy().to_string(),
        host: config.host,
        port: config.port,
        tenant: config.tenant,
        database: config.database,
        default_database: default_storage_database(),
        user: config.user,
        password_configured: !config.password.is_empty(),
        runtime_log_retention_days: config.runtime_log_retention_days,
        setup_required: *state
            .storage_setup_required
            .lock()
            .map_err(|_| "Storage setup state lock is poisoned".to_string())?,
        writable: *state
            .storage_ready
            .lock()
            .map_err(|_| "Storage state lock is poisoned".to_string())?,
        error,
    })
}

// Remote storage can acknowledge a write before its next read observes it.
// Retry the read-back check briefly so a successful migration is not reported as failed.
fn verify_storage_snapshot(
    engine: &mut StorageEngine,
    expected: &AppStore,
    expected_log_count: usize,
) -> Result<(), String> {
    let mut last_error = String::from("Target storage has not returned migration data");
    for attempt in 1..=5 {
        match (engine.load(), engine.log_count()) {
            (Ok(actual), Ok(actual_log_count)) => {
                let actual = actual.unwrap_or_default();
                if actual.instances.len() == expected.instances.len()
                    && actual.vault.len() == expected.vault.len()
                    && actual_log_count == expected_log_count
                {
                    return Ok(());
                }
                last_error = format!(
                    "Instances {} -> {}, Vault {} -> {}, Logs {} -> {}",
                    expected.instances.len(),
                    actual.instances.len(),
                    expected.vault.len(),
                    actual.vault.len(),
                    expected_log_count,
                    actual_log_count,
                );
            }
            (Err(error), _) | (_, Err(error)) => {
                last_error = error;
            }
        }
        if attempt < 5 {
            let delay = 250_u64 * 2_u64.pow((attempt - 1) as u32);
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
    Err(format!("Target storage validation failed: {last_error}"))
}

#[tauri::command]
async fn configure_storage(
    state: State<'_, DesktopState>,
    mut config: StorageConfig,
) -> Result<StorageStatus, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        state.ensure_storage_configurable()?;
        if config.password.is_empty() {
            if let Ok(current) = state.storage_config.lock() {
                if current.mode == config.mode
                    && current.host == config.host
                    && current.user == config.user
                {
                    config.password = current.password.clone();
                }
            }
        }
        let allowed = [
            "sqlite_embedded",
            "seekdb_embedded",
            "seekdb_server",
            "oceanbase_server",
        ];
        if !allowed.contains(&config.mode.as_str()) {
            return Err("Unsupported desktop storage type".to_string());
        }
        config.setup_completed = true;
        normalize_storage_database(&mut config);
        if !(1..=3_650).contains(&config.runtime_log_retention_days) {
            config.runtime_log_retention_days = default_runtime_log_retention_days();
        }
        if matches!(config.mode.as_str(), "sqlite_embedded" | "seekdb_embedded") {
            if config.path.trim().is_empty() {
                config.path = if config.mode == "sqlite_embedded" {
                    state.data_dir.to_string_lossy().to_string()
                } else {
                    state.data_dir.join("seekdb").to_string_lossy().to_string()
                };
            }
            if !Path::new(&config.path).is_absolute() {
                return Err("Embedded storage data directory must use an absolute path".to_string());
            }
            fs::create_dir_all(&config.path).map_err(|error| error.to_string())?;
        }
        if matches!(config.mode.as_str(), "seekdb_server" | "oceanbase_server")
            && config.host.trim().is_empty()
        {
            return Err("Server mode requires a host address".to_string());
        }
        let current_config = state
            .storage_config
            .lock()
            .map_err(|_| "Storage config lock is poisoned".to_string())?
            .clone();
        let effective_mode = state
            .effective_storage_mode
            .lock()
            .map_err(|_| "Storage state lock is poisoned".to_string())?
            .clone();
        let same_target = effective_mode == config.mode
            && match config.mode.as_str() {
                "sqlite_embedded" => current_config.path == config.path,
                "seekdb_embedded" => {
                    current_config.path == config.path && current_config.database == config.database
                }
                _ => {
                    current_config.host == config.host
                        && current_config.port == config.port
                        && current_config.tenant == config.tenant
                        && current_config.database == config.database
                        && current_config.user == config.user
                }
            };
        let previous_ready = {
            let mut ready = state
                .storage_ready
                .lock()
                .map_err(|_| "Storage state lock is poisoned".to_string())?;
            let previous = *ready;
            *ready = false;
            previous
        };
        let result = (|| -> Result<StorageStatus, String> {
            let snapshot = sanitized_store(
                &state
                    .data
                    .lock()
                    .map_err(|_| "State lock is poisoned".to_string())?
                    .clone(),
            );
            write_storage_backup(&state.data_dir, &snapshot)?;
            let mut engine = if config.mode == "sqlite_embedded" {
                StorageEngine::Sqlite(sqlite_database_path(&state.data_dir, &config))
            } else {
                ensure_seekdb_runtime(&state.data_dir)?;
                let pending = state.data_dir.join("storage.pending.json");
                fs::write(
                    &pending,
                    serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
                let bridge = SeekDbBridge::open(&pending, &state.data_dir);
                let _ = fs::remove_file(&pending);
                StorageEngine::SeekDb(bridge?)
            };
            let final_snapshot = state
                .data
                .lock()
                .map_err(|_| "State lock is poisoned".to_string())?
                .clone();
            let source_log_count = if previous_ready {
                state
                    .storage
                    .lock()
                    .map_err(|_| "Storage lock is poisoned".to_string())?
                    .log_count()?
            } else {
                0
            };
            engine
                .save_core(&final_snapshot)
                .map_err(|error| format!("Failed to write target storage instances and vault: {error}"))?;
            if !same_target && previous_ready {
                engine
                    .clear_logs()
                    .map_err(|error| format!("Failed to clean target storage old logs: {error}"))?;
                let mut before_sequence = None;
                // Stream logs in bounded pages so a storage switch does not load the full history.
                loop {
                    let page = state
                        .storage
                        .lock()
                        .map_err(|_| "Storage lock is poisoned".to_string())?
                        .query_logs(&LogQuery {
                            before_sequence,
                            after_sequence: None,
                            limit: LOG_CLEANUP_BATCH_SIZE,
                        })?;
                    if page.entries.is_empty() {
                        break;
                    }
                    before_sequence = page.entries.iter().map(|log| log.sequence).min();
                    engine
                        .append_logs(&page.entries)
                        .map_err(|error| format!("Failed to migrate log pagination: {error}"))?;
                    if !page.has_more {
                        break;
                    }
                }
            }
            // A fresh installation legitimately reads back as an empty store.
            verify_storage_snapshot(&mut engine, &final_snapshot, source_log_count)?;
            let mut data_guard = state
                .data
                .lock()
                .map_err(|_| "State lock is poisoned".to_string())?;
            let pending_logs = std::mem::take(&mut data_guard.logs);
            if let Err(error) = engine.append_logs(&pending_logs) {
                data_guard.logs = pending_logs;
                return Err(error);
            }
            let expected_log_count = source_log_count + pending_logs.len();
            if let Err(error) =
                verify_storage_snapshot(&mut engine, &final_snapshot, expected_log_count)
            {
                data_guard.logs = pending_logs;
                return Err(format!("Target storage validation failed during switch: {error}"));
            }
            if let Err(error) = write_local_credentials(
                &state.credentials_path,
                &LocalCredentials {
                    storage_password: config.password.clone(),
                },
            ) {
                data_guard.logs = pending_logs;
                return Err(error);
            }
            if let Err(error) = write_storage_config(&state.config_path, &config) {
                data_guard.logs = pending_logs;
                return Err(error);
            }
            // Publish the new engine only after data, credentials, and configuration are durable.
            *state
                .storage
                .lock()
                .map_err(|_| "Storage lock is poisoned".to_string())? = engine;
            *state
                .storage_config
                .lock()
                .map_err(|_| "Storage config lock is poisoned".to_string())? = config.clone();
            *state
                .effective_storage_mode
                .lock()
                .map_err(|_| "Storage state lock is poisoned".to_string())? = config.mode.clone();
            *state
                .storage_error
                .lock()
                .map_err(|_| "Storage error lock is poisoned".to_string())? = None;
            *state
                .storage_ready
                .lock()
                .map_err(|_| "Storage state lock is poisoned".to_string())? = true;
            *state
                .storage_setup_required
                .lock()
                .map_err(|_| "Storage setup state lock is poisoned".to_string())? = false;
            drop(data_guard);
            storage_status_value(&state)
        })();
        if result.is_err() {
            if let Ok(mut ready) = state.storage_ready.lock() {
                *ready = previous_ready;
            }
        }
        result
    })
    .await
    .map_err(|error| error.to_string())?
}

// ---------------------------------------------------------------------------
// System info & Docker requirements
// ---------------------------------------------------------------------------

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
        app_name: "AgentSeek".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
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

#[tauri::command]
fn check_instance_docker_requirements(
    state: State<'_, DesktopState>,
    instance_id: String,
) -> Result<Option<String>, String> {
    let instance = instance_by_id(&state, &instance_id)?;
    if let Some(message) = docker_compose_check(Path::new(&instance.work_dir)) {
        state.log(
            Some(&instance.id),
            &instance.name,
            "install",
            "error",
            &message,
            Some("docker --version && docker compose version --short && docker info".to_string()),
        );
        Ok(Some(format!("{} instance startup process exited, please check lifecycle logs", instance.name)))
    } else {
        Ok(None)
    }
}
