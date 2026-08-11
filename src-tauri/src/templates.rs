// Template catalog: cache management, template commands, and instance
// creation from templates (describe → interactive create with auto-fed
// answers).
//
// NOTE: this file is include!()-ed into lib.rs; all `use` items are
// inherited from the parent module.

// ---------------------------------------------------------------------------
// Template cache management
// ---------------------------------------------------------------------------

fn display_name(template_id: &str) -> String {
    template_id
        .split('/')
        .next_back()
        .unwrap_or(template_id)
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Return the template cache directory path.
fn template_cache_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| Path::new(&home).join(".cookiecutters").join("agentseek"))
}

/// Read templates directly from the cached `templates/index.json`.
///
/// This mirrors how `agentseek create --list-templates` resolves templates
/// internally: the registry is `templates/index.json` keyed by `<type>/<name>`
/// with the description as the value.
fn read_template_index() -> Result<Vec<TemplateInfo>, String> {
    let index_path = template_cache_dir()
        .ok_or_else(|| "Cannot determine template cache directory".to_string())?
        .join("templates")
        .join("index.json");
    let raw = fs::read_to_string(&index_path)
        .map_err(|e| format!("Failed to read templates/index.json: {e}"))?;
    let map: HashMap<String, String> = serde_json::from_str(&raw)
        .map_err(|e| format!("Invalid templates/index.json: {e}"))?;
    Ok(map
        .into_iter()
        .map(|(id, description)| {
            let framework = id.split('/').next().unwrap_or_default().to_string();
            TemplateInfo {
                id: id.clone(),
                name: display_name(&id),
                description,
                framework,
            }
        })
        .collect())
}

/// Build CLI flags for `--template-repo` and `--checkout` based on the current cache state.
/// These flags tell the CLI to resolve templates from the cached repository rather than its
/// built-in catalog lock.
pub(crate) fn template_repo_flags(cfg: &TemplateConfig) -> Vec<String> {
    let mut flags = vec![
        "--template-repo".to_string(),
        cfg.repo_url.clone(),
    ];
    if let Some(sha) = template_cache_commit_sha() {
        flags.push("--checkout".to_string());
        flags.push(sha);
    }
    flags
}

/// Parse a template repository URL and extract (git_clone_url, checkout_ref).
///
/// Supported formats:
/// - `https://github.com/org/repo.git` → checkout empty (auto: latest release → main/master)
/// - `https://github.com/org/repo/tree/<branch>` → checkout = branch
/// - `https://github.com/org/repo/releases/tag/<tag>` → checkout = tag
pub(crate) fn parse_template_repo_url(url: &str) -> Result<(String, String), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Template repository URL cannot be empty".to_string());
    }
    if !url.starts_with("https://") {
        return Err("Template repository URL must use HTTPS".to_string());
    }
    // /releases/tag/<tag>
    if let Some(pos) = url.find("/releases/tag/") {
        let repo_url = url[..pos].to_string();
        let tag = url[pos + 14..].trim_end_matches('/').to_string();
        if tag.is_empty() {
            return Err("Release tag is empty".to_string());
        }
        // Validate repo_url has a repo path (contains org/repo)
        validate_repo_url_structure(&repo_url)?;
        return Ok((repo_url, tag));
    }
    // /releases or /releases/latest → auto
    if url.ends_with("/releases") || url.ends_with("/releases/latest") {
        let repo_url = url
            .strip_suffix("/releases")
            .or_else(|| url.strip_suffix("/releases/latest"))
            .unwrap_or(url)
            .to_string();
        validate_repo_url_structure(&repo_url)?;
        return Ok((repo_url, String::new()));
    }
    // /tree/<branch>
    if let Some(pos) = url.find("/tree/") {
        let repo_url = url[..pos].to_string();
        let branch = url[pos + 6..].trim_end_matches('/').to_string();
        if branch.is_empty() {
            return Err("Branch name is empty".to_string());
        }
        validate_repo_url_structure(&repo_url)?;
        return Ok((repo_url, branch));
    }
    // Plain HTTPS repo URL (auto-detect)
    validate_repo_url_structure(url)?;
    Ok((url.to_string(), String::new()))
}

/// Validate that a repo URL looks like a proper GitHub-style repository URL.
fn validate_repo_url_structure(url: &str) -> Result<(), String> {
    let url = url.trim_end_matches('/').trim_end_matches(".git");
    // Must contain at least org/repo after github.com
    let after_host = if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://gitlab.com/") {
        rest
    } else {
        // Non-GitHub/GitLab hosts: ensure the path has at least two segments.
        let path = url.trim_end_matches('/').trim_end_matches(".git");
        let after_scheme = path.strip_prefix("https://").unwrap_or(path);
        let path_start = after_scheme.find('/').map(|p| &after_scheme[p + 1..]).unwrap_or("");
        let segments: Vec<&str> = path_start.split('/').filter(|s| !s.is_empty()).collect();
        if segments.len() < 2 {
            return Err(format!("URL does not appear to be a git repository: {url}"));
        }
        return Ok(());
    };
    let parts: Vec<&str> = after_host.split('/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!("URL does not appear to be a git repository: {url}"));
    }
    Ok(())
}

/// Derive GitHub releases API URL from a repository URL.
/// e.g. "https://github.com/org/repo" -> "https://api.github.com/repos/org/repo/releases/latest"
fn github_releases_api(repo_url: &str) -> Option<String> {
    let repo_url = repo_url.trim_end_matches('/').trim_end_matches(".git");
    // Extract org/repo from URL like https://github.com/org/repo or git@github.com:org/repo.git
    let path = if let Some(rest) = repo_url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = repo_url.strip_prefix("http://github.com/") {
        rest
    } else { repo_url.strip_prefix("git@github.com:")? };
    let parts: Vec<&str> = path.split('/').take(2).collect();
    if parts.len() != 2 {
        return None;
    }
    Some(format!("https://api.github.com/repos/{}/{}/releases/latest", parts[0], parts[1]))
}

/// Query the GitHub releases API for the latest release tag of the template repository.
fn latest_template_release_tag(api_url: &str) -> Option<String> {
    let output = configured_command(curl_program())
        .args([
            "-fsSL",
            "--connect-timeout",
            "5",
            "--max-time",
            "15",
            "--retry",
            "2",
            "-H",
            "Accept: application/vnd.github+json",
            api_url,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json.get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Return the currently checked-out template version (git tag) from the cache directory.
fn current_template_version() -> Option<String> {
    let cache_dir = template_cache_dir()?;
    if !cache_dir.is_dir() {
        return None;
    }
    let output = configured_command("git")
        .args(["describe", "--tags", "--exact-match", "HEAD"])
        .current_dir(&cache_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if tag.is_empty() { None } else { Some(tag) }
}

/// Return the full commit SHA of the template cache.
fn template_cache_commit_sha() -> Option<String> {
    let cache_dir = template_cache_dir()?;
    if !cache_dir.is_dir() {
        return None;
    }
    let output = configured_command("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cache_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.len() == 40 { Some(sha) } else { None }
}

/// Clone the template repository into the cache directory.
///
/// - If `checkout` is non-empty: `git clone --depth 1 --branch {checkout} {repo_url}`
/// - If `checkout` is empty: try latest GitHub release tag → fall back to `main` branch.
fn clone_template_repo(repo_url: &str, checkout: &str) -> Result<(), String> {
    let Some(cache_dir) = template_cache_dir() else {
        return Err("HOME environment variable is not set".to_string());
    };
    let effective_ref: String;
    let effective_ref: &str = if !checkout.is_empty() {
        checkout
    } else {
        // Auto-detect: try latest release tag first, then fall back to "main".
        if let Some(api_url) = github_releases_api(repo_url) {
            if let Some(tag) = latest_template_release_tag(&api_url) {
                eprintln!("[templates] Auto-detected latest release: {tag}");
                effective_ref = tag;
                &effective_ref
            } else {
                "main"
            }
        } else {
            "main"
        }
    };
    eprintln!("[templates] Cloning {repo_url} (ref: {effective_ref})...");
    // Clone into a temporary sibling directory first, then atomically replace
    // the cache. A failed clone must never leave the existing cache deleted
    // (otherwise the template list becomes empty until the next successful fetch).
    let parent = cache_dir
        .parent()
        .ok_or_else(|| format!("Cannot resolve template cache parent directory: {}", cache_dir.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("Failed to create template cache parent directory: {error}"))?;
    let staging = parent.join(format!(".agentseek-templates-{}", unique_stamp()));
    let clone_ok = configured_command("git")
        .args(["clone", "--quiet", "--depth", "1", "--branch", effective_ref, repo_url, staging.to_str().unwrap_or("")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !clone_ok {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "Failed to clone template repository: {repo_url} (ref: {effective_ref})"
        ));
    }
    // Write .template-source marker: repo_url + commit SHA for cache validation.
    let marker_content = if let Some(sha) = template_cache_commit_sha_in(&staging) {
        format!("{repo_url}|{sha}")
    } else {
        repo_url.to_string()
    };
    let _ = fs::write(staging.join(".template-source"), &marker_content);
    let stale = cache_dir.join(format!(".stale-{}", unique_stamp()));
    if cache_dir.is_dir()
        && fs::rename(&cache_dir, &stale).is_err() {
            // Fall back to a plain delete when the rename is not possible.
            let _ = fs::remove_dir_all(&cache_dir);
        }
    let swap = fs::rename(&staging, &cache_dir);
    let _ = fs::remove_dir_all(&stale);
    swap.map_err(|error| format!("Failed to move template repository into place: {error}"))?;
    eprintln!("[templates] Clone complete at {}", cache_dir.display());
    Ok(())
}

/// Resolve the checked-out commit SHA inside a (possibly not-yet-active) cache directory.
fn template_cache_commit_sha_in(cache_dir: &Path) -> Option<String> {
    let output = configured_command("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cache_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.len() == 40 { Some(sha) } else { None }
}

/// Delete the template cache and re-fetch from the configured repo.
fn update_template_cache(cfg: &TemplateConfig) -> Result<(), String> {
    clone_template_repo(&cfg.repo_url, &cfg.checkout)
}

/// Ensure the template cache exists. Called lazily when listing templates.
/// Skips cloning if the cache is already up-to-date (marker matches).
fn ensure_template_cache(cfg: &TemplateConfig) {
    let Some(cache_dir) = template_cache_dir() else {
        return;
    };
    let marker = cache_dir.join(".template-source");
    let expected_prefix = &cfg.repo_url;
    if cache_dir.is_dir() && marker.is_file() {
        if let Ok(stored) = fs::read_to_string(&marker) {
            if stored.trim().starts_with(expected_prefix) {
                return; // Cache is valid.
            }
        }
    }
    let _ = clone_template_repo(&cfg.repo_url, &cfg.checkout);
}

// ---------------------------------------------------------------------------
// Template commands
// ---------------------------------------------------------------------------

/// Read TemplateConfig from DB, falling back to defaults.
fn read_template_config(state: &DesktopState) -> TemplateConfig {
    let mut engine = match state.storage.lock() {
        Ok(e) => e,
        Err(_) => return TemplateConfig::default(),
    };
    TemplateConfig {
        repo_url: engine
            .get_app_config("template.repo_url")
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| TemplateConfig::default().repo_url),
        checkout: engine
            .get_app_config("template.checkout")
            .ok()
            .flatten()
            .unwrap_or_default(),
        catalog_url: engine
            .get_app_config("template.catalog_url")
            .ok()
            .flatten()
            .unwrap_or_default(),
    }
}

#[tauri::command]
async fn list_templates(state: State<'_, DesktopState>, force: Option<bool>) -> Result<Vec<TemplateInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = read_template_config(&state);
        if force.unwrap_or(false) {
            update_template_cache(&cfg)?;
        } else {
            ensure_template_cache(&cfg);
        }
        let templates = read_template_index()?;
        state.log(
            None,
            "AgentSeek Desktop",
            "lifecycle",
            "info",
            format!(
                "Template cache at ~/.cookiecutters/agentseek\n{} templates loaded from templates/index.json",
                templates.len()
            ),
            None,
        );
        Ok(templates)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn check_template_update(state: State<'_, DesktopState>) -> Result<TemplateUpdateCheck, String> {
    let no_check = TemplateUpdateCheck {
        current_version: String::new(),
        latest_version: String::new(),
        has_update: false,
    };
    let cfg = read_template_config(state.inner());
    let Some(api_url) = github_releases_api(&cfg.repo_url) else {
        return Ok(no_check);
    };
    let current = current_template_version().unwrap_or_default();
    let latest = latest_template_release_tag(&api_url).unwrap_or_default();
    let has_update = !latest.is_empty() && !current.is_empty() && current != latest;
    Ok(TemplateUpdateCheck {
        current_version: current,
        latest_version: latest,
        has_update,
    })
}

#[tauri::command]
async fn update_templates(state: State<'_, DesktopState>) -> Result<Vec<TemplateInfo>, String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cfg = read_template_config(&state);
        update_template_cache(&cfg)?;
        let templates = read_template_index()?;
        state.log(
            None,
            "AgentSeek Desktop",
            "lifecycle",
            "info",
            format!(
                "Templates updated to {}, {} templates loaded from templates/index.json",
                current_template_version().unwrap_or_default(),
                templates.len()
            ),
            None,
        );
        Ok(templates)
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Return the user-facing template settings (raw URL + catalog URL).
#[tauri::command]
fn get_template_settings(state: State<'_, DesktopState>) -> Result<TemplateConfig, String> {
    let mut engine = state.storage.lock().map_err(|_| "Engine lock is poisoned".to_string())?;
    // Prefer raw user-entered URL for display; fall back to parsed git URL.
    let repo_url = engine
        .get_app_config("template.repo_url_raw")
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            engine
                .get_app_config("template.repo_url")
                .ok()
                .flatten()
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| TemplateConfig::default().repo_url);
    let catalog_url = engine
        .get_app_config("template.catalog_url")
        .ok()
        .flatten()
        .unwrap_or_default();
    Ok(TemplateConfig { repo_url, checkout: String::new(), catalog_url })
}

/// Save template settings: parse repo URL → derive git URL + checkout, validate, persist.
#[tauri::command]
fn save_template_settings(state: State<'_, DesktopState>, cfg: TemplateConfig) -> Result<(), String> {
    let raw = cfg.repo_url.trim();
    let catalog = cfg.catalog_url.trim();
    // Parse and validate the repo URL, extracting git clone URL and checkout ref.
    let (git_url, checkout) = parse_template_repo_url(raw)?;
    state.ensure_storage_ready()?;
    {
        let mut engine = state.storage.lock().map_err(|_| "Engine lock is poisoned".to_string())?;
        engine.set_app_config("template.repo_url", &git_url)?;
        engine.set_app_config("template.repo_url_raw", raw)?;
        engine.set_app_config("template.checkout", &checkout)?;
        engine.set_app_config("template.catalog_url", catalog)?;
    }
    // Clear stale template cache so the next list_templates fetches the new repo.
    if let Some(cache_dir) = template_cache_dir() {
        let _ = fs::remove_dir_all(&cache_dir);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Describe-driven interactive create
// ---------------------------------------------------------------------------

/// Parse the non-underscore variable names from `agentseek create --describe` output.
/// Underscore-prefixed variables (e.g. `_agentseek_source_*`, `_gateway_port`) are
/// filtered out because cookiecutter does not prompt for them interactively.
fn parse_describe_variables(describe_output: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let mut in_vars = false;
    for line in describe_output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Cookiecutter variables") {
            in_vars = true;
            continue;
        }
        if !in_vars {
            continue;
        }
        // Stop at empty line or section separator.
        if trimmed.is_empty() || trimmed.starts_with("──") || trimmed.starts_with("--") {
            break;
        }
        // Line format: `  name: value` or `  name:` (empty value).
        if let Some((name, _)) = trimmed.split_once(':') {
            let name = name.trim();
            if !name.is_empty() && !name.starts_with('_') {
                vars.push(name.to_string());
            }
        }
    }
    vars
}

/// Build the interactive answers string for `agentseek create` (one line per variable).
/// - `project_name` → instance name
/// - `project_slug` → instance name slugified (lowercase; spaces/dashes → underscores)
/// - `author` → "AgentSeek Desktop"
/// - `*_port` → resolved port from `resolve_describe_ports` (conflict-aware)
/// - others → empty line (cookiecutter uses default)
fn build_create_answers(
    variables: &[String],
    instance_name: &str,
    resolved_ports: &std::collections::HashMap<String, u16>,
) -> String {
    let instance_slug = instance_name
        .to_lowercase()
        .replace([' ', '-'], "_");
    let mut lines = Vec::with_capacity(variables.len());
    for var in variables {
        let value = match var.as_str() {
            "project_name" => instance_name.to_string(),
            "project_slug" => instance_slug.clone(),
            "author" => "AgentSeek Desktop".to_string(),
            v if v.ends_with("_port") => resolved_ports
                .get(&v.to_ascii_uppercase())
                .map(|p| p.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        lines.push(value);
    }
    // Trailing newline ensures the last line is read by cookiecutter.
    lines.join("\n") + "\n"
}

/// Temporarily patch underscore-prefixed port variables in the template's
/// `cookiecutter.json` (e.g. `_gateway_port` in deepagents/default) so that
/// conflict-resolved ports are rendered correctly. Returns a guard that
/// restores the original file on drop.
///
/// The target file is resolved from the `Path:` line of describe output: the
/// CLI renders from its own cache (`~/.cookiecutters/.as/...`), never from the
/// app-side `~/.cookiecutters/agentseek` clone.
fn patch_cookiecutter_json_for_underscore_ports(
    describe_output: &str,
    resolved_ports: &std::collections::HashMap<String, u16>,
) -> Result<Option<CookiecutterPatchGuard>, String> {
    // Fast path: no underscore-prefixed port to inject, nothing to patch.
    let has_underscore_port = resolved_ports.keys().any(|key| key.starts_with('_'));
    if !has_underscore_port {
        return Ok(None);
    }

    let template_dir = describe_output
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Path: "))
        .ok_or_else(|| "Failed to locate template path in describe output".to_string())?;
    let cookiecutter_json = PathBuf::from(template_dir).join("cookiecutter.json");
    if !cookiecutter_json.exists() {
        return Ok(None);
    }

    let original_content = fs::read_to_string(&cookiecutter_json)
        .map_err(|e| format!("Failed to read cookiecutter.json: {e}"))?;
    let mut doc: serde_json::Value = serde_json::from_str(&original_content)
        .map_err(|e| format!("Failed to parse cookiecutter.json: {e}"))?;
    let obj = match doc.as_object_mut() {
        Some(obj) => obj,
        None => return Ok(None),
    };

    let mut patched = Vec::new();
    for (key, value) in obj.iter_mut() {
        if key.starts_with('_') && key.ends_with("_port") {
            let env_key = key.to_ascii_uppercase();
            if let Some(&new_port) = resolved_ports.get(&env_key) {
                let old_value = value.clone();
                *value = serde_json::Value::Number(
                    serde_json::Number::from(new_port),
                );
                patched.push((key.clone(), old_value));
            }
        }
    }

    if patched.is_empty() {
        return Ok(None);
    }

    let new_content = serde_json::to_string_pretty(&doc)
        .map_err(|e| format!("Failed to serialize cookiecutter.json: {e}"))?;
    fs::write(&cookiecutter_json, new_content)
        .map_err(|e| format!("Failed to write cookiecutter.json: {e}"))?;

    Ok(Some(CookiecutterPatchGuard {
        path: cookiecutter_json,
        original_content,
    }))
}

/// Guard that restores the original `cookiecutter.json` on drop.
struct CookiecutterPatchGuard {
    path: PathBuf,
    original_content: String,
}

impl Drop for CookiecutterPatchGuard {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.original_content);
    }
}

/// Serializes patch-and-create so concurrent creations of the same template
/// cannot interleave cookiecutter.json patches (the CLI template cache is
/// shared across instances).
static COOKIECUTTER_PATCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let cfg = read_template_config(&state);
        let repo_flags = template_repo_flags(&cfg);
        let mut describe_args = vec!["create".to_string(), "--describe".to_string()];
        describe_args.extend(repo_flags.iter().cloned());
        describe_args.push(input.template_id.clone());
        let describe_refs: Vec<&str> = describe_args.iter().map(String::as_str).collect();
        let describe_result = run_cli_with_input(&describe_refs, None, None)
            .map_err(|error| format!("Failed to read template description: {error}"))?;
        if describe_result.code != 0 {
            return Err(format!("Failed to read template description: {}", describe_result.output));
        }
        let reserved = collect_assigned_ports(&state, None);
        // Debug: log raw describe output for port resolution diagnosis.
        state.log(
            Some(&instance_id),
            &input.name,
            "install",
            "info",
            format!("Describe output:\n{}", describe_result.output),
            None,
        );
        let (mut resolved_ports, mut port_changes) =
            resolve_describe_ports(&describe_result.output, &reserved)?;

        // Parse variable list from describe output (non-underscore variables).
        let variables = parse_describe_variables(&describe_result.output);
        if variables.is_empty() {
            return Err("Failed to parse template variables from describe output".to_string());
        }

        // Serialize patch-and-create so concurrent creations of the same
        // template cannot interleave cookiecutter.json patches.
        let _patch_lock = COOKIECUTTER_PATCH_LOCK
            .lock()
            .map_err(|_| "Template patch lock is poisoned".to_string())?;

        // Temporarily patch underscore-prefixed port variables in cookiecutter.json
        // (e.g. _gateway_port in deepagents/default) for conflict-resolved ports.
        let guard = patch_cookiecutter_json_for_underscore_ports(
            &describe_result.output,
            &resolved_ports,
        )?;

        // Build interactive answers for cookiecutter prompts.
        let answers = build_create_answers(&variables, input.name.trim(), &resolved_ports);

        // Debug: log parsed variables, resolved ports, and generated answers.
        state.log(
            Some(&instance_id),
            &input.name,
            "install",
            "info",
            format!(
                "Template answers debug:\n  variables: {:?}\n  resolved_ports: {:?}\n  answers:\n{}",
                variables, resolved_ports, answers
            ),
            None,
        );

        let create_started = Instant::now();
        let mut create_args = vec!["create".to_string()];
        create_args.extend(repo_flags);
        create_args.push(input.template_id.clone());
        // No --no-input: interactive mode with auto-fed answers.
        let create_refs: Vec<&str> = create_args.iter().map(String::as_str).collect();
        let result = match run_cli_with_input(
            &create_refs,
            Some(&staging),
            Some(&answers),
        ) {
            Ok(result) => result,
            Err(error) => {
                drop(guard); // Restore cookiecutter.json before returning.
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        drop(guard); // Restore cookiecutter.json after create completes.
        drop(_patch_lock); // Patch-and-create critical section is over.
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
        // Patch Dockerfile to add PyPI mirror fallback for slow
        // connections in China. Tests actual download speed from pypi.org
        // and falls back to mirrors.aliyun.com when direct access is too slow.
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
                for port in resolved_ports.values() {
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

        // Patch source files (e.g. langgraph_dev.py) when port conflicts
        // caused lifecycle.toml URLs to use different ports than the
        // cookiecutter-rendered defaults.
        patch_source_ports_for_conflicts(&target, &port_changes);

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
                        } else if extract_url_port(&entry.value) == Some(change.old_port)
                            && LOOPBACK_URL_PREFIXES
                                .iter()
                                .any(|prefix| entry.value.starts_with(prefix))
                        {
                            // URL contains old port — sync update (e.g. BUB_AG_UI_AGENT_URL).
                            // Only host-reachable URLs are rewritten; container-internal
                            // endpoints (e.g. http://phoenix:6006/v1/traces) must keep the
                            // in-network port.
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
            .any(|e| e.key.eq_ignore_ascii_case("LANGSMITH_TRACING"))
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
            project_name: None,
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
        Ok(PrepareInstanceResult {
            instance,
            env,
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests_templates {
    use super::*;

    #[test]
    fn template_config_defaults() {
        let cfg = TemplateConfig::default();
        assert_eq!(cfg.repo_url, "https://github.com/agentseek-ai/agentseek-templates.git");
        assert!(cfg.checkout.is_empty());
        assert!(cfg.catalog_url.is_empty());
    }

    #[test]
    fn github_releases_api_from_https() {
        let api = github_releases_api("https://github.com/org/repo");
        assert_eq!(api.unwrap(), "https://api.github.com/repos/org/repo/releases/latest");
    }

    #[test]
    fn github_releases_api_from_https_with_git_suffix() {
        let api = github_releases_api("https://github.com/org/repo.git");
        assert_eq!(api.unwrap(), "https://api.github.com/repos/org/repo/releases/latest");
    }

    #[test]
    fn github_releases_api_from_ssh() {
        let api = github_releases_api("git@github.com:org/repo.git");
        assert_eq!(api.unwrap(), "https://api.github.com/repos/org/repo/releases/latest");
    }

    #[test]
    fn github_releases_api_from_non_github() {
        assert!(github_releases_api("https://gitlab.com/org/repo").is_none());
    }

    #[test]
    fn parse_plain_repo_url() {
        let (git_url, checkout) = parse_template_repo_url("https://github.com/agentseek-ai/agentseek-templates.git").unwrap();
        assert_eq!(git_url, "https://github.com/agentseek-ai/agentseek-templates.git");
        assert!(checkout.is_empty());
    }

    #[test]
    fn parse_tree_url() {
        let (git_url, checkout) = parse_template_repo_url("https://github.com/agentseek-ai/agentseek-templates/tree/main").unwrap();
        assert_eq!(git_url, "https://github.com/agentseek-ai/agentseek-templates");
        assert_eq!(checkout, "main");
    }

    #[test]
    fn parse_tree_url_deep_branch() {
        let (git_url, checkout) = parse_template_repo_url("https://github.com/kic635/agentseek-templates/tree/feat/langchain-relay-observability").unwrap();
        assert_eq!(git_url, "https://github.com/kic635/agentseek-templates");
        assert_eq!(checkout, "feat/langchain-relay-observability");
    }

    #[test]
    fn parse_releases_tag_url() {
        let (git_url, checkout) = parse_template_repo_url("https://github.com/agentseek-ai/agentseek-templates/releases/tag/v0.1.0").unwrap();
        assert_eq!(git_url, "https://github.com/agentseek-ai/agentseek-templates");
        assert_eq!(checkout, "v0.1.0");
    }

    #[test]
    fn parse_releases_url_auto() {
        let (git_url, checkout) = parse_template_repo_url("https://github.com/agentseek-ai/agentseek-templates/releases").unwrap();
        assert_eq!(git_url, "https://github.com/agentseek-ai/agentseek-templates");
        assert!(checkout.is_empty());
    }

    #[test]
    fn parse_releases_latest_url_auto() {
        let (git_url, checkout) = parse_template_repo_url("https://github.com/agentseek-ai/agentseek-templates/releases/latest").unwrap();
        assert_eq!(git_url, "https://github.com/agentseek-ai/agentseek-templates");
        assert!(checkout.is_empty());
    }

    #[test]
    fn reject_empty_url() {
        assert!(parse_template_repo_url("").is_err());
        assert!(parse_template_repo_url("   ").is_err());
    }

    #[test]
    fn reject_non_https() {
        assert!(parse_template_repo_url("http://github.com/org/repo").is_err());
        assert!(parse_template_repo_url("git@github.com:org/repo.git").is_err());
    }

    #[test]
    fn reject_empty_branch() {
        assert!(parse_template_repo_url("https://github.com/org/repo/tree/").is_err());
    }

    #[test]
    fn reject_empty_tag() {
        assert!(parse_template_repo_url("https://github.com/org/repo/releases/tag/").is_err());
    }

    #[test]
    fn parse_describe_variables_filters_underscore_prefix() {
        let describe_output = r#"  Template: langchain/default
  ────────────────────────────────────────────────────────────
  Description: LangChain create_agent plus CopilotKit middleware with AgentSeek lifecycle spec.
  Path: /Users/sunchong/.cookiecutters/.l/QuJgJg6gwIqMioIj5hdUBJ4ujSTeN0c_6dU6oxCydto/template/templates/langchain/default
  Cookiecutter variables (13):
    project_name: My LangChain Agent
    project_slug: {{ cookiecutter.project_name.lower().replace(' ', '_').replace('-', '_') }}
    author: Your Name
    system_prompt: You are a helpful UI assistant. Build visual responses using the available co...
    default_model: openai:Pro/zai-org/GLM-5.1
    gateway_port: 8089
    frontend_port: 5174
    copilotkit_port: 4001
    _agentseek_source_path:
    _agentseek_source_path_posix:
    _agentseek_source_path_shell:
    _agentseek_source_url: https://github.com/ob-labs/agentseek.git
    _agentseek_source_ref: 883addad1e2993c4be6fc8ba053f87f25fb5057a
"#;
        let vars = parse_describe_variables(describe_output);
        assert_eq!(
            vars,
            vec![
                "project_name",
                "project_slug",
                "author",
                "system_prompt",
                "default_model",
                "gateway_port",
                "frontend_port",
                "copilotkit_port",
            ]
        );
    }

    #[test]
    fn parse_describe_variables_handles_underscore_port() {
        let describe_output = r#"  Template: deepagents/default
  Cookiecutter variables (10):
    project_name: My DeepAgent
    project_slug: {{ cookiecutter.project_name.lower().replace(' ', '_').replace('-', '_') }}
    author: Your Name
    system_prompt: You are a pragmatic engineering assistant.
    default_model: openai:gpt-4o-mini
    _gateway_port: 18088
    _agentseek_source_path:
    _agentseek_source_path_posix:
    _agentseek_source_url: https://github.com/ob-labs/agentseek.git
    _agentseek_source_ref: 883addad1e2993c4be6fc8ba053f87f25fb5057a
"#;
        let vars = parse_describe_variables(describe_output);
        assert_eq!(
            vars,
            vec!["project_name", "project_slug", "author", "system_prompt", "default_model"]
        );
        // _gateway_port is filtered out (underscore prefix).
    }

    #[test]
    fn build_create_answers_generates_correct_values() {
        let variables = vec![
            "project_name".to_string(),
            "project_slug".to_string(),
            "author".to_string(),
            "system_prompt".to_string(),
            "default_model".to_string(),
            "gateway_port".to_string(),
            "frontend_port".to_string(),
            "copilotkit_port".to_string(),
        ];
        let mut resolved_ports = std::collections::HashMap::new();
        resolved_ports.insert("GATEWAY_PORT".to_string(), 8089);
        resolved_ports.insert("FRONTEND_PORT".to_string(), 5174);
        resolved_ports.insert("COPILOTKIT_PORT".to_string(), 4001);

        let answers = build_create_answers(&variables, "my_test_agent", &resolved_ports);
        let lines: Vec<&str> = answers.lines().collect();

        assert_eq!(lines.len(), 8);
        assert_eq!(lines[0], "my_test_agent"); // project_name
        assert_eq!(lines[1], "my_test_agent"); // project_slug (slugified)
        assert_eq!(lines[2], "AgentSeek Desktop"); // author
        assert_eq!(lines[3], ""); // system_prompt (default)
        assert_eq!(lines[4], ""); // default_model (default)
        assert_eq!(lines[5], "8089"); // gateway_port
        assert_eq!(lines[6], "5174"); // frontend_port
        assert_eq!(lines[7], "4001"); // copilotkit_port
    }

    #[test]
    fn build_create_answers_slugifies_instance_name() {
        let variables = vec!["project_name".to_string(), "project_slug".to_string()];
        let resolved_ports = std::collections::HashMap::new();

        let answers = build_create_answers(&variables, "My Test-Agent", &resolved_ports);
        let lines: Vec<&str> = answers.lines().collect();

        assert_eq!(lines[0], "My Test-Agent"); // project_name (original)
        assert_eq!(lines[1], "my_test_agent"); // project_slug (slugified)
    }

    #[test]
    fn parse_describe_variables_empty_output_returns_empty() {
        assert!(parse_describe_variables("").is_empty());
        assert!(parse_describe_variables("Template: langchain/default\n").is_empty());
    }

    #[test]
    fn patch_underscore_ports_uses_path_from_describe_and_restores() {
        // Build a fake template dir the describe `Path:` line points at.
        let tmp = std::env::temp_dir().join(format!(
            "cookiecutter-patch-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let cookiecutter_json = tmp.join("cookiecutter.json");
        fs::write(&cookiecutter_json, r#"{
  "project_name": "My Agent",
  "_gateway_port": "18088",
  "_agentseek_source_url": "https://github.com/example/repo.git"
}
"#)
        .unwrap();

        let describe_output = format!(
            "  Template: deepagents/default\n  Path: {}\n  Cookiecutter variables (1):\n    project_name: My Agent\n",
            tmp.display()
        );
        let mut resolved_ports = std::collections::HashMap::new();
        resolved_ports.insert("_GATEWAY_PORT".to_string(), 18089);

        let guard = patch_cookiecutter_json_for_underscore_ports(&describe_output, &resolved_ports)
            .expect("patch should succeed");
        let guard = guard.expect("patch guard should be created");

        // While the guard is alive, the file must carry the resolved port.
        let patched = fs::read_to_string(&cookiecutter_json).unwrap();
        assert!(patched.contains("\"_gateway_port\": 18089"), "got: {patched}");
        assert!(patched.contains("\"_agentseek_source_url\""), "must keep other keys: {patched}");

        drop(guard);

        // After the guard drops, the original file must be restored.
        let restored = fs::read_to_string(&cookiecutter_json).unwrap();
        assert!(restored.contains("\"_gateway_port\": \"18088\""), "got: {restored}");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn patch_underscore_ports_skips_without_underscore_ports() {
        let describe_output = "  Template: langchain/default\n  Path: /nonexistent\n";
        let mut resolved_ports = std::collections::HashMap::new();
        resolved_ports.insert("GATEWAY_PORT".to_string(), 8089);

        let guard = patch_cookiecutter_json_for_underscore_ports(describe_output, &resolved_ports)
            .expect("no underscore port means no patch");
        assert!(guard.is_none());
    }

    fn templates_root() -> std::path::PathBuf {
        std::path::PathBuf::from(
            env::var("HOME").expect("HOME env var"),
        )
        .join(".cookiecutters/agentseek/templates")
    }
    fn render_template(raw: &str) -> String {
        // Resolve {% if %} / {% else %} / {% endif %} blocks by keeping the
        // else branch (source path is empty for normal deployments).
        let mut result = String::with_capacity(raw.len());
        let mut skip_lines = false; // inside an if-branch (before else)
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("{% if ") {
                skip_lines = true;
                continue;
            }
            if trimmed.starts_with("{% else %}") {
                skip_lines = false;
                continue;
            }
            if trimmed.starts_with("{% endif %}") {
                skip_lines = false;
                continue;
            }
            if trimmed.starts_with("{%") {
                // Skip other Jinja2 tags (e.g. {% for %}).
                continue;
            }
            if skip_lines {
                continue; // skip the if-branch
            }
            // We're outside any if/else block or in the else branch.
            // Replace Jinja2 variables with example values.
            let rendered = trimmed
                .replace("{{ cookiecutter.project_slug }}", "my_agent")
                .replace("{{ cookiecutter.frontend_port }}", "5175")
                .replace("{{ cookiecutter.gateway_port }}", "8089")
                .replace("{{ cookiecutter.llm_model_id }}", "Qwen/Qwen2.5-7B-Instruct")
                .replace("{{ cookiecutter.embedding_model_id }}", "BAAI/bge-small-zh-v1.5")
                .replace("{{ cookiecutter.llm_model_path }}", "./models/llm")
                .replace("{{ cookiecutter.embedding_model_path }}", "./models/embedding");
            // Preserve original indentation.
            let indent = &line[..line.len() - line.trim_start().len()];
            result.push_str(indent);
            result.push_str(&rendered);
            result.push('\n');
        }
        result
    }
    fn setup_template_instance(template_id: &str, test_dir: &std::path::Path) -> bool {
        let template_path = templates_root()
            .join(template_id)
            .join("{{cookiecutter.project_slug}}");
        if !template_path.is_dir() {
            eprintln!("skipping: template not cached at {}", template_path.display());
            eprintln!("run `agentseek create {} --no-input` to cache templates", template_id);
            return false;
        }
        // Recursively copy and render template files.
        fn copy_recursive(src: &std::path::Path, dst: &std::path::Path) {
            for entry in fs::read_dir(src).expect("read template dir") {
                let entry = entry.expect("dir entry");
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let src_path = entry.path();
                if src_path.is_dir() {
                    // Replace {{cookiecutter.project_slug}} with my_agent.
                    let dir_name = if name_str.contains("{{") {
                        "my_agent".to_string()
                    } else {
                        name_str.to_string()
                    };
                    let dst_path = dst.join(&dir_name);
                    fs::create_dir_all(&dst_path).expect("mkdir");
                    copy_recursive(&src_path, &dst_path);
                } else {
                    let dst_path = dst.join(&*name);
                    // Try to read as UTF-8 text; if that fails, copy binary.
                    match fs::read_to_string(&src_path) {
                        Ok(raw) => {
                            let rendered = render_template(&raw);
                            fs::write(&dst_path, rendered).expect("write rendered file");
                        }
                        Err(_) => {
                            // Binary file (e.g. PNG images) — copy verbatim.
                            fs::copy(&src_path, &dst_path).expect("copy binary file");
                        }
                    }
                }
            }
        }
        copy_recursive(&template_path, test_dir);
        true
    }
    fn apply_all_patches(dir: &std::path::Path) {
        patch_convert_models_if_needed(dir);
        patch_agent_async_if_needed(dir);
        patch_dockerfile_apt_mirror_if_needed(dir);
        patch_dockerfile_mirrors_if_needed(dir);
        patch_langgraph_cors_if_needed(dir);
    }
    #[test]
    fn template_bub_default() {
        let dir = patch_test_dir("tpl-bub-default");
        if !setup_template_instance("bub/default", &dir) { return; }
        apply_all_patches(&dir);
        let dockerfile = find_file_recursive(&dir, "Dockerfile", 5);
        if let Some(path) = dockerfile {
            let content = fs::read_to_string(&path).expect("read");
            // bub/default Dockerfile has no apt-get update or uv sync.
            assert!(!content.contains("mirrors.aliyun.com/pypi/simple/"));
        }
        // No langgraph.json or convert_models.py in this template.
        assert!(find_file_recursive(&dir, "langgraph.json", 5).is_none());
        assert!(find_file_recursive(&dir, "convert_models.py", 5).is_none());
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_deepagents_default() {
        let dir = patch_test_dir("tpl-da-default");
        if !setup_template_instance("deepagents/default", &dir) { return; }
        apply_all_patches(&dir);
        let dockerfile = find_file_recursive(&dir, "Dockerfile", 5);
        if let Some(path) = dockerfile {
            let content = fs::read_to_string(&path).expect("read");
            // deepagents/default Dockerfile has no apt-get update or uv sync.
            assert!(!content.contains("mirrors.aliyun.com/pypi/simple/"));
        }
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_deepagents_research() {
        let dir = patch_test_dir("tpl-da-research");
        if !setup_template_instance("deepagents/research", &dir) { return; }
        apply_all_patches(&dir);
        // Has langgraph.json but no cors section — patch should be a no-op.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_deepagents_sandbox() {
        let dir = patch_test_dir("tpl-da-sandbox");
        if !setup_template_instance("deepagents/sandbox", &dir) { return; }
        apply_all_patches(&dir);
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_deepagents_content_builder() {
        let dir = patch_test_dir("tpl-da-cb");
        if !setup_template_instance("deepagents/content-builder", &dir) { return; }
        apply_all_patches(&dir);
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_langchain_default() {
        let dir = patch_test_dir("tpl-lc-default");
        if !setup_template_instance("langchain/default", &dir) { return; }
        apply_all_patches(&dir);
        // Dockerfile: should have apt mirror + GitHub/PyPI mirror fallbacks.
        let dockerfile = find_file_recursive(&dir, "Dockerfile", 5).expect("Dockerfile");
        let content = fs::read_to_string(&dockerfile).expect("read");
        assert!(content.contains("timeout 60"), "apt mirror patch not applied");
        assert!(content.contains("mirrors.aliyun.com"), "apt mirror fallback missing");
        assert!(content.contains("mirrors.aliyun.com/pypi/simple/"), "PyPI mirror fallback missing");
        // Idempotency: applying again should not change anything.
        let before = content.clone();
        apply_all_patches(&dir);
        let after = fs::read_to_string(&dockerfile).expect("read");
        assert_eq!(before, after, "patches should be idempotent");
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_langchain_cli_remote() {
        let dir = patch_test_dir("tpl-lc-cli");
        if !setup_template_instance("langchain/cli-remote", &dir) { return; }
        apply_all_patches(&dir);
        let dockerfile = find_file_recursive(&dir, "Dockerfile", 5);
        if let Some(path) = dockerfile {
            let content = fs::read_to_string(&path).expect("read");
            // cli-remote Dockerfile has no apt-get update or uv sync.
            assert!(!content.contains("mirrors.aliyun.com/pypi/simple/"));
        }
        // Has langgraph.json but no cors section.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_langchain_agentic_rag() {
        let dir = patch_test_dir("tpl-lc-rag");
        if !setup_template_instance("langchain/agentic-rag", &dir) { return; }
        apply_all_patches(&dir);
        // agent.py has no langchain-huggingface dependency — patch should be no-op.
        let agent_py = find_file_recursive(&dir, "agent.py", 5).expect("agent.py");
        let content = fs::read_to_string(&agent_py).expect("read");
        assert!(!content.contains("_patched_agenerate"), "async shim should not be added");
        // langgraph.json has no cors section — should remain valid JSON.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_langchain_agentic_rag_hybrid() {
        let dir = patch_test_dir("tpl-lc-rag-hybrid");
        if !setup_template_instance("langchain/agentic-rag-hybrid", &dir) { return; }
        apply_all_patches(&dir);
        // langgraph.json: should have CORS patched to allow any origin.
        let langgraph = find_file_recursive(&dir, "langgraph.json", 5).expect("langgraph.json");
        let content = fs::read_to_string(&langgraph).expect("read");
        let json: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        let cors = &json["http"]["cors"];
        assert_eq!(cors["allow_origin_regex"].as_str(), Some("^https?://.*$"));
        assert!(cors.get("allow_origins").is_none());
        assert_eq!(cors["allow_methods"], serde_json::json!(["*"]));
        assert_eq!(cors["allow_headers"], serde_json::json!(["*"]));
        // agent.py has no langchain-huggingface dependency — patch should be no-op.
        if let Some(path) = find_file_recursive(&dir, "agent.py", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(!content.contains("_patched_agenerate"));
        }
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_langchain_agentic_rag_openvino() {
        let dir = patch_test_dir("tpl-lc-rag-openvino");
        if !setup_template_instance("langchain/agentic-rag-openvino", &dir) { return; }
        apply_all_patches(&dir);
        // convert_models.py: template already uses optimum-cli — patch should skip.
        let convert_py = find_file_recursive(&dir, "convert_models.py", 5)
            .expect("convert_models.py");
        let content = fs::read_to_string(&convert_py).expect("read");
        assert!(content.contains("optimum-cli"), "should still have optimum-cli");
        // agent.py should have the async shim.
        let agent_py = find_file_recursive(&dir, "agent.py", 5).expect("agent.py");
        let content = fs::read_to_string(&agent_py).expect("read");
        assert!(content.contains("_patched_agenerate"), "async shim not added");
        // langgraph.json has no cors section.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn template_langchain_markdown_messages() {
        let dir = patch_test_dir("tpl-lc-md");
        if !setup_template_instance("langchain/markdown-messages", &dir) { return; }
        apply_all_patches(&dir);
        // langgraph.json has no cors section.
        if let Some(path) = find_file_recursive(&dir, "langgraph.json", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        }
        // agent.py has no langchain-huggingface dependency.
        if let Some(path) = find_file_recursive(&dir, "agent.py", 5) {
            let content = fs::read_to_string(&path).expect("read");
            assert!(!content.contains("_patched_agenerate"));
        }
        fs::remove_dir_all(&dir).ok();
    }
}
