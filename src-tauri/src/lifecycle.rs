// Lifecycle TOML synchronization: service endpoint enrichment,
// port/config synchronization between lifecycle.toml, .env, and docker-compose.yml.

fn service_display_name(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "app" | "frontend" | "web" => "Frontend".to_string(),
        "gateway" | "agent" => "Agent / Gateway".to_string(),
        "copilotkit" => "CopilotKit Runtime".to_string(),
        "ctx" | "contextseek" => "ContextSeek API".to_string(),
        "studio" | "langsmith" => "LangSmith Studio".to_string(),
        _ => name.to_string(),
    }
}

fn service_kind(name: &str) -> (&'static str, bool) {
    match name.to_ascii_lowercase().as_str() {
        "app" | "frontend" | "web" => ("web", true),
        "studio" | "langsmith" | "phoenix" => ("web", false),
        "gateway" | "agent" => ("protocol", false),
        "copilotkit" | "backend" | "langgraph" | "ctx" | "contextseek" => ("api", false),
        "database" | "db" | "seekdb" | "oceanbase" => ("database", false),
        _ => ("other", false),
    }
}

fn replace_url_port(url: &str, port: u16) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_end + 3;
    let remainder = &url[authority_start..];
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let host_end = if authority.starts_with('[') {
        authority.find(']').map(|index| index + 1)
    } else {
        authority.rfind(':').or(Some(authority.len()))
    };
    let Some(host_end) = host_end else {
        return url.to_string();
    };
    let host = &authority[..host_end];
    if host.is_empty() {
        return url.to_string();
    }
    format!(
        "{}{}:{}{}",
        &url[..authority_start],
        host,
        port,
        &remainder[authority_end..]
    )
}

fn service_env_port(name: &str, env: &HashMap<String, String>) -> Option<u16> {
    let normalized = name.to_ascii_lowercase();
    let mut candidates: Vec<String> = match normalized.as_str() {
        "app" | "frontend" | "web" => vec!["FRONTEND_PORT", "APP_PORT", "WEB_PORT"],
        "gateway" | "agent" => vec![
            "BUB_AG_UI_PORT",
            "AG_UI_PORT",
            "GATEWAY_PORT",
            "AGENT_PORT",
            "BACKEND_PORT",
        ],
        "copilotkit" | "runtime" => vec!["COPILOTKIT_PORT", "RUNTIME_PORT"],
        "backend" | "langgraph" => vec!["BACKEND_PORT", "LANGGRAPH_PORT", "API_PORT"],
        "ctx" | "contextseek" => vec!["CTX_SERVER_PORT", "CONTEXTSEEK_PORT"],
        "studio" | "langsmith" => vec!["STUDIO_PORT", "LANGSMITH_PORT"],
        "phoenix" => vec!["PHOENIX_PORT"],
        _ => Vec::new(),
    }
    .into_iter()
    .map(str::to_string)
    .collect();
    candidates.push(format!("{}_PORT", name.to_ascii_uppercase()));
    candidates.into_iter().find_map(|key| {
        env.get(&key)
            .and_then(|value| value.trim().parse::<u16>().ok())
            .filter(|port| *port > 0)
    })
}

/// Re-calibrate an instance from its on-disk lifecycle.toml / .env.
/// Returns `true` when any calibratable field drifted, so callers can decide
/// whether the change is worth persisting back to the DB.
fn enrich_service_endpoints(instance: &mut InstanceRecord) -> bool {
    let path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    let Some(manifest) = fs::read_to_string(path)
        .ok()
        .and_then(|content| toml::from_str::<LifecycleManifest>(&content).ok())
    else {
        return false;
    };
    // Snapshot the calibratable fields to detect drift.
    let before = (
        instance.project_name.clone(),
        instance.lifecycle_version,
        instance.service_endpoints.clone(),
        instance.agent_url.clone(),
        instance.ui_url.clone(),
        instance.studio_url.clone(),
    );
    if !manifest.name.trim().is_empty() {
        instance.project_name = Some(manifest.name.clone());
    }
    instance.lifecycle_version = (manifest.version > 0).then_some(manifest.version);
    let env_path = instance
        .env_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&instance.work_dir).join(".env"));
    let env = fs::read_to_string(env_path)
        .ok()
        .map(|content| {
            parse_env(&content)
                .into_iter()
                .map(|entry| (entry.key, entry.value))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut services = manifest
        .services
        .into_iter()
        .filter(|(_, service)| !service.url.trim().is_empty())
        .map(|(name, mut service)| {
            service.url = service_env_port(&name, &env)
                .map(|port| replace_url_port(&service.url, port))
                .unwrap_or(service.url);
            (name, service)
        })
        .collect::<Vec<_>>();
    services.sort_by_key(|(name, _)| match name.to_ascii_lowercase().as_str() {
        "gateway" | "agent" => 0,
        "app" | "frontend" | "web" => 1,
        "copilotkit" | "runtime" => 2,
        "studio" | "langsmith" => 3,
        _ => 4,
    });
    instance.service_endpoints = services
        .iter()
        .map(|(name, service)| {
            // Prefer kind/primary from lifecycle.toml; fall back to name inference.
            let (inferred_kind, inferred_primary) = service_kind(name);
            let kind = if service.kind.is_empty() {
                inferred_kind
            } else {
                &service.kind
            };
            let primary = service.primary.unwrap_or(inferred_primary);
            let display_name = if service.display.is_empty() {
                service_display_name(name)
            } else {
                service.display.clone()
            };
            ServiceEndpoint {
                name: display_name,
                url: service.url.clone(),
                kind: kind.to_string(),
                primary,
            }
        })
        .collect();
    for (name, service) in &services {
        match name.to_ascii_lowercase().as_str() {
            "gateway" | "agent" => instance.agent_url = Some(service.url.clone()),
            "app" | "frontend" | "web" => instance.ui_url = Some(service.url.clone()),
            "studio" | "langsmith" => instance.studio_url = Some(service.url.clone()),
            _ => {}
        }
    }
    let changed = before != (
        instance.project_name.clone(),
        instance.lifecycle_version,
        instance.service_endpoints.clone(),
        instance.agent_url.clone(),
        instance.ui_url.clone(),
        instance.studio_url.clone(),
    );
    if changed {
        instance.updated_at = timestamp();
    }
    changed
}

fn lifecycle_section_service(header: &str) -> Option<String> {
    let header = header.strip_prefix('[')?.strip_suffix(']')?.trim();
    let (group, service) = header.split_once('.')?;
    matches!(group.trim(), "services" | "checks").then(|| {
        service
            .trim()
            .trim_matches(|character| matches!(character, '\'' | '"'))
            .to_string()
    })
}

fn lifecycle_section_env_key(header: &str) -> Option<String> {
    let header = header.strip_prefix('[')?.strip_suffix(']')?.trim();
    let (group, key) = header.split_once('.')?;
    (group.trim() == "env").then(|| {
        key.trim()
            .trim_matches(|character| matches!(character, '\'' | '"'))
            .to_string()
    })
}

fn replace_toml_string_line(
    line: &str,
    keys: &[&str],
    update: impl FnOnce(&str) -> String,
) -> String {
    let Some(equals) = line.find('=') else {
        return line.to_string();
    };
    if !keys.contains(&line[..equals].trim()) {
        return line.to_string();
    }
    let value = &line[equals + 1..];
    let Some(quote_start) = value.find(['\'', '"']) else {
        return line.to_string();
    };
    let quote = value.as_bytes()[quote_start];
    let Some(quote_end_offset) = value.as_bytes()[quote_start + 1..]
        .iter()
        .position(|candidate| *candidate == quote)
    else {
        return line.to_string();
    };
    let value_start = equals + 1 + quote_start + 1;
    let value_end = value_start + quote_end_offset;
    let current = &line[value_start..value_end];
    let updated = update(current);
    if updated == current {
        return line.to_string();
    }
    format!("{}{}{}", &line[..value_start], updated, &line[value_end..])
}

fn replace_lifecycle_url_line(line: &str, port: u16) -> String {
    replace_toml_string_line(line, &["url", "target"], |url| {
        if url.contains("${") {
            return url.to_string();
        }
        replace_url_port(url, port)
    })
}

fn toml_basic_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

fn replace_lifecycle_name_line(line: &str, project_name: &str) -> String {
    let (body, line_ending) = line
        .strip_suffix("\r\n")
        .map(|body| (body, "\r\n"))
        .or_else(|| line.strip_suffix('\n').map(|body| (body, "\n")))
        .unwrap_or((line, ""));
    let Some(equals) = body.find('=') else {
        return line.to_string();
    };
    if body[..equals].trim() != "name" {
        return line.to_string();
    }

    let value = &body[equals + 1..];
    let mut quote = None;
    let mut escaped = false;
    let mut comment_start = None;
    for (index, character) in value.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some(active) if character == active => quote = None,
            Some(_) => {}
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character == '#' => {
                comment_start = Some(index);
                break;
            }
            None => {}
        }
    }

    let value_end = comment_start.unwrap_or(value.len());
    let old_value = &value[..value_end];
    let leading_len = old_value.len() - old_value.trim_start().len();
    let trailing_start = old_value.trim_end().len();
    let leading = &old_value[..leading_len];
    let trailing = &old_value[trailing_start..];
    let comment = &value[value_end..];
    format!(
        "{}{}{}{}{}{}",
        &body[..=equals],
        leading,
        toml_basic_string(project_name),
        trailing,
        comment,
        line_ending
    )
}

fn synchronize_lifecycle_project_name_content(content: &str, project_name: &str) -> String {
    let mut in_root = true;
    let mut found = false;
    let mut output = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if in_root && trimmed.starts_with('[') {
            in_root = false;
        }
        if in_root && !found {
            let updated = replace_lifecycle_name_line(line, project_name);
            found = updated != line
                || line
                    .split_once('=')
                    .is_some_and(|(key, _)| key.trim() == "name");
            output.push_str(&updated);
        } else {
            output.push_str(line);
        }
    }
    output
}


/// Rewrites hardcoded port mappings in docker-compose.yml to `${ENV_KEY:-ORIGINAL}:CONTAINER` variable references.
fn sync_docker_compose_port_mappings(content: &str, entries: &[EnvVariable]) -> String {
    let env_keys: HashSet<String> = entries
        .iter()
        .map(|e| e.key.to_ascii_uppercase())
        .collect();

    let mut updated = content.to_string();
    let mut current_service: Option<String> = None;

    for line in content.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();

        if indent == 2 && trimmed.ends_with(':') && !trimmed.starts_with('-') {
            let name = trimmed.trim_end_matches(':');
            if name != "services" {
                current_service = Some(name.to_string());
            } else {
                current_service = None;
            }
        }

        if trimmed.starts_with("- ") && trimmed.contains(':') {
            if let Some(ref service_name) = current_service {
                let env_key = format!("{}_PORT", service_name.to_ascii_uppercase());
                if !env_keys.contains(&env_key) {
                    continue;
                }
                let mapping = trimmed
                    .trim_start_matches("- ")
                    .trim()
                    .trim_matches('"');
                if mapping.contains("${") {
                    continue;
                }
                if let Some(colon_pos) = mapping.rfind(':') {
                    let container_port = &mapping[colon_pos + 1..];
                    if !container_port.chars().all(|c| c.is_ascii_digit()) {
                        continue;
                    }
                    let host_part = &mapping[..colon_pos];
                    if let Some(host_colon) = host_part.rfind(':') {
                        let original_host_port = &host_part[host_colon + 1..];
                        if !original_host_port.chars().all(|c| c.is_ascii_digit()) {
                            continue;
                        }
                        let prefix = &host_part[..host_colon];
                        let old_mapping = format!(
                            "{}:{}:{}",
                            prefix, original_host_port, container_port
                        );
                        let new_mapping = format!(
                            "{}:${{{}:-{}}}:{}",
                            prefix, env_key, original_host_port, container_port
                        );
                        updated = updated.replace(&old_mapping, &new_mapping);
                    } else if host_part.chars().all(|c| c.is_ascii_digit()) {
                        let old_mapping = format!("{}:{}", host_part, container_port);
                        let new_mapping = format!(
                            "${{{}:-{}}}:{}",
                            env_key, host_part, container_port
                        );
                        updated = updated.replace(&old_mapping, &new_mapping);
                    }
                }
            }
        }
    }

    updated
}

/// Injects `NPM_CONFIG_REGISTRY` into docker-compose services whose command
/// runs `npm install`, so that dependency resolution uses a faster mirror
/// instead of the default registry.npmjs.org (which is extremely slow from
/// mainland China and causes frontend health-check timeouts).
fn sync_docker_compose_npm_mirror(content: &str) -> String {
    const ENV_KEY: &str = "NPM_CONFIG_REGISTRY";

    // Only act when the compose file actually contains npm install commands.
    if !content.contains("npm install") {
        return content.to_string();
    }

    let stripped = content.replace("npm install --no-package-lock", "npm install");

    // If the mirror env-var is already present somewhere, return after stripping.
    if stripped.contains(ENV_KEY) {
        return stripped;
    }

    // For every `environment:` block that belongs to a service whose command
    // runs `npm install`, inject `NPM_CONFIG_REGISTRY`.
    let mut lines: Vec<String> = stripped.lines().map(str::to_string).collect();
    let mut env_indices: Vec<usize> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "environment:" {
            continue;
        }
        let env_indent = line.len() - line.trim_start().len();
        // The service key sits one indent level above `environment:`.
        let service_indent = env_indent.saturating_sub(2);
        // Scan the entire service scope (all lines indented deeper than
        // the service key) for `npm install`.
        let mut service_has_npm_install = false;
        for next in lines.iter().skip(i + 1) {
            if next.trim().is_empty() {
                continue;
            }
            let next_indent = next.len() - next.trim_start().len();
            if next_indent <= service_indent {
                break;
            }
            if next.contains("npm install") {
                service_has_npm_install = true;
                break;
            }
        }
        if service_has_npm_install {
            env_indices.push(i);
        }
    }

    // Insert in reverse order so earlier indices remain valid.
    for &idx in env_indices.iter().rev() {
        let indent = lines[idx].len() - lines[idx].trim_start().len();
        let prefix: String = lines[idx].chars().take(indent).collect();
        let new_line = format!("{}  {}: \"{}\"", prefix, ENV_KEY, NPM_REGISTRY_MIRROR);
        lines.insert(idx + 1, new_line);
    }

    let mut result = lines.join("\n");
    if content.ends_with('\n') {
        result.push('\n');
    }
    result
}

fn synchronize_instance_project_name(
    root: &Path,
    project_name: &str,
) -> Result<Option<PathBuf>, String> {
    let lifecycle_path = root.join(".agentseek/lifecycle.toml");
    if !lifecycle_path.is_file() {
        return Ok(None);
    }
    let content = fs::read_to_string(&lifecycle_path)
        .map_err(|error| format!("Unable to read {}: {error}", lifecycle_path.display()))?;
    let updated = synchronize_lifecycle_project_name_content(&content, project_name);
    if updated == content {
        return Ok(None);
    }
    fs::write(&lifecycle_path, updated)
        .map_err(|error| format!("Unable to write {}: {error}", lifecycle_path.display()))?;
    Ok(Some(lifecycle_path))
}

fn synchronize_lifecycle_content(content: &str, root: &[EnvVariable]) -> String {
    let env = root
        .iter()
        .map(|entry| (entry.key.clone(), entry.value.clone()))
        .collect::<HashMap<_, _>>();
    let mut service = None;
    let mut env_key = None;
    let mut output = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            service = lifecycle_section_service(trimmed);
            env_key = lifecycle_section_env_key(trimmed);
        }
        if let Some(port) = service
            .as_deref()
            .and_then(|name| service_env_port(name, &env))
        {
            output.push_str(&replace_lifecycle_url_line(line, port));
        } else if let Some(value) = env_key
            .as_deref()
            .filter(|key| is_local_service_port_key(key))
            .and_then(|key| env.get(key))
        {
            output.push_str(&replace_toml_string_line(line, &["default"], |_| {
                value.clone()
            }));
        } else {
            output.push_str(line);
        }
    }
    output
}

fn extract_lifecycle_service_ports(content: &str) -> Vec<(String, u16)> {
    let mut result = Vec::new();
    let mut current_service: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            current_service = None;
        }
        if let Some(service) = trimmed
            .strip_prefix("[services.")
            .and_then(|r| r.strip_suffix(']'))
        {
            current_service = Some(service.to_string());
            continue;
        }
        if let Some(ref service) = current_service {
            if let Some(eq_pos) = trimmed.find('=') {
                let after_eq = trimmed[eq_pos + 1..].trim();
                let url = after_eq.trim_matches('"');
                if let Some(port) = extract_url_port(url) {
                    result.push((service.to_ascii_uppercase(), port));
                }
            }
        }
    }
    result
}

fn insert_command_port(line: &str, port: u16) -> Option<String> {
    let tokens = command_tokens(line);
    // npm run <script> — inject "--", "--port", "<port>" so npm passes
    // the flag through to the underlying vite/uvicorn process.
    let is_npm_run = tokens.first().is_some_and(|t| t == "npm")
        && tokens.get(1).is_some_and(|t| t == "run");
    if is_npm_run {
        if let Some(bracket) = line.rfind(']') {
            let before = &line[..bracket];
            let port_arg = format!(", \"--\", \"--port\", \"{port}\"");
            return Some(format!("{before}{port_arg}]"));
        }
    }
    let is_shell_wrapped = tokens.first().is_some_and(|t| t == "sh")
        && tokens.len() >= 3;
    if is_shell_wrapped {
        // For sh -lc "..." wrapped commands, inject --port INTO the inner
        // command string (last quoted element) instead of adding a separate
        // array element that would be passed to sh itself.
        if let Some(bracket) = line.rfind(']') {
            let before_bracket = &line[..bracket];
            if let Some(last_quote) = before_bracket.rfind('"') {
                let before_quote = &line[..last_quote];
                let rest = &line[last_quote..];
                return Some(format!("{before_quote} --port {port}{rest}"));
            }
        }
    }
    if let Some(bracket) = line.rfind(']') {
        let before = &line[..bracket];
        let port_arg = format!(", \"--port\", \"{port}\"");
        return Some(format!("{before}{port_arg}]"));
    }
    if let Some(quote) = line.rfind('"') {
        let before = &line[..quote];
        let after = &line[quote..];
        return Some(format!("{before} --port {port}{after}"));
    }
    None
}

fn command_tokens(line: &str) -> Vec<String> {
    let after_eq = line.split_once('=').map(|(_, rest)| rest).unwrap_or(line);
    let mut quoted: Vec<String> = Vec::new();
    let mut in_quote = false;
    let mut cur = String::new();
    for ch in after_eq.chars() {
        if ch == '"' {
            if in_quote {
                quoted.push(std::mem::take(&mut cur));
            }
            in_quote = !in_quote;
        } else if in_quote {
            cur.push(ch);
        }
    }
    match quoted.len() {
        0 => Vec::new(),
        1 => quoted[0]
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        _ => quoted,
    }
}

fn accepts_port_flag(tokens: &[String]) -> bool {
    // npm run <script> — pass --port through to the underlying tool via `--`
    if tokens.len() >= 2 && tokens[0] == "npm" && tokens[1] == "run" {
        return true;
    }
    tokens.iter().any(|t| {
        let lower = t.to_ascii_lowercase();
        lower == "langgraph" || lower == "vite" || lower == "uvicorn"
            || lower == "agentseek-api"
            || lower.contains("langgraph") || lower.contains("uvicorn")
            || lower.contains("agentseek-api")
    })
}

fn remove_command_port(line: &str) -> Option<String> {
    let flag_pos = line.find("--port")?;
    let after_flag = &line[flag_pos..];
    let after_comma = after_flag.find(',').unwrap_or(after_flag.len());
    let after_comma_s = &after_flag[after_comma..];
    if let Some(q) = after_comma_s.find('"') {
        let rest = &after_comma_s[q + 1..];
        if let Some(q2) = rest.find('"') {
            let port_str = &rest[..q2];
            if port_str.parse::<u16>().is_ok() {
                // Try removing with npm `--` separator first
                let target_with_sep = format!(", \"--\", \"--port\", \"{}\"", port_str);
                let cleaned = line.replace(&target_with_sep, "");
                if cleaned != line {
                    return Some(cleaned);
                }
                let target = format!(", \"--port\", \"{}\"", port_str);
                let cleaned = line.replace(&target, "");
                if cleaned != line {
                    return Some(cleaned);
                }
            }
        }
    }
    let after_flag_trimmed = after_flag.strip_prefix("--port").unwrap_or(after_flag);
    let after_space = after_flag_trimmed.trim_start();
    let num_end = after_space
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_space.len());
    if num_end > 0 {
        let port_str = &after_space[..num_end];
        if port_str.parse::<u16>().is_ok() {
            let target = format!(" --port {}", port_str);
            let cleaned = line.replace(&target, "");
            if cleaned != line {
                return Some(cleaned);
            }
        }
    }
    None
}

fn sync_process_command_ports(content: &str, entries: &[EnvVariable]) -> String {
    let lifecycle_ports = extract_lifecycle_service_ports(content);
    let mut updated = content.to_string();
    let mut current_process: Option<String> = None;
    let mut seen_processes: HashSet<String> = HashSet::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[processes.") {
            current_process = trimmed
                .strip_prefix("[processes.")
                .and_then(|r| r.strip_suffix(']'))
                .map(|n| n.to_string());
            if let Some(ref name) = current_process {
                seen_processes.insert(name.to_ascii_uppercase());
            }
        } else if trimmed.starts_with('[') {
            current_process = None;
        } else if let Some(ref proc_name) = current_process {
            if !trimmed.starts_with("command") {
                continue;
            }
            let port_key = format!("{}_PORT", proc_name.to_ascii_uppercase());
            let new_port = entries
                .iter()
                .find(|e| e.key.to_ascii_uppercase() == port_key)
                .and_then(|e| e.value.trim().parse::<u16>().ok())
                .or_else(|| {
                    lifecycle_ports
                        .iter()
                        .find(|(name, _)| *name == proc_name.to_ascii_uppercase())
                        .map(|(_, port)| *port)
                });
            if !accepts_port_flag(&command_tokens(line)) {
                if line.contains("--port") {
                    if let Some(cleaned) = remove_command_port(line) {
                        updated = updated.replace(line, &cleaned);
                    }
                }
                continue;
            }

            if let Some(new_port) = new_port {
                // For npm run commands, migrate bare --port (no -- separator)
                // to the correct "--", "--port", "<port>" format.
                let tokens = command_tokens(line);
                let is_npm_run = tokens.first().is_some_and(|t| t == "npm")
                    && tokens.get(1).is_some_and(|t| t == "run");
                if is_npm_run && line.contains("--port") && !line.contains("\"--\"") {
                    if let Some(cleaned) = remove_command_port(line) {
                        if let Some(inserted) = insert_command_port(&cleaned, new_port) {
                            updated = updated.replace(line, &inserted);
                            continue;
                        }
                    }
                }
                if let Some(flag_pos) = line.find("--port") {
                    let after_flag = &line[flag_pos..];
                    let after_comma = after_flag.find(',').unwrap_or(after_flag.len());
                    if let Some(replaced) = (|| {
                        let after_comma_s = &after_flag[after_comma..];
                        let q = after_comma_s.find('"')?;
                        let rest = &after_comma_s[q + 1..];
                        let q2 = rest.find('"')?;
                        let current_port_str = &rest[..q2];
                        let current_port = current_port_str.parse::<u16>().ok()?;
                        if current_port != new_port {
                            let old = format!("--port\", \"{current_port}\"");
                            let new_val = format!("--port\", \"{new_port}\"");
                            return Some(line.replace(&old, &new_val));
                        }
                        None
                    })() {
                        updated = updated.replace(line, &replaced);
                        continue;
                    }
                    let after_flag_trimmed =
                        after_flag.strip_prefix("--port").unwrap_or(after_flag);
                    let after_space = after_flag_trimmed.trim_start();
                    if let Some(replaced) = (|| {
                        let num_end = after_space
                            .find(|c: char| !c.is_ascii_digit())
                            .unwrap_or(after_space.len());
                        if num_end == 0 {
                            return None;
                        }
                        let current_port_str = &after_space[..num_end];
                        let current_port = current_port_str.parse::<u16>().ok()?;
                        if current_port != new_port {
                            let old = format!("--port {current_port}");
                            let new_val = format!("--port {new_port}");
                            return Some(line.replace(&old, &new_val));
                        }
                        None
                    })() {
                        updated = updated.replace(line, &replaced);
                    }
                } else {
                    if let Some(replaced) = insert_command_port(line, new_port) {
                        updated = updated.replace(line, &replaced);
                    }
                }
            }
        }
    }

    updated
}

fn synchronize_instance_port_configs(
    root: &Path,
    entries: &[EnvVariable],
) -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::new();
    let lifecycle_path = root.join(".agentseek/lifecycle.toml");
    if lifecycle_path.is_file() {
        let content = fs::read_to_string(&lifecycle_path)
            .map_err(|error| format!("Failed to read {}: {error}", lifecycle_path.display()))?;
        let updated = synchronize_lifecycle_content(&content, entries);
        let updated = sync_process_command_ports(&updated, entries);
        if updated != content {
            fs::write(&lifecycle_path, updated)
                .map_err(|error| format!("Failed to write {}: {error}", lifecycle_path.display()))?;
            written.push(lifecycle_path);
        }
    }

    let frontend_example_path = root.join("frontend/.env.example");
    if frontend_example_path.is_file() {
        let frontend_env_path = root.join("frontend/.env");
        let example = parse_env(
            &fs::read_to_string(&frontend_example_path).map_err(|error| {
                format!("Failed to read {}: {error}", frontend_example_path.display())
            })?,
        );
        let existing_content = fs::read_to_string(&frontend_env_path).ok();
        let existing = existing_content
            .as_deref()
            .map(parse_env)
            .unwrap_or_default();
        let existing_by_key = existing
            .iter()
            .map(|entry| (entry.key.to_ascii_uppercase(), entry))
            .collect::<HashMap<_, _>>();
        let example_keys = example
            .iter()
            .map(|entry| entry.key.to_ascii_uppercase())
            .collect::<HashSet<_>>();
        let mut frontend = example
            .into_iter()
            .map(|mut entry| {
                if let Some(saved) = existing_by_key.get(&entry.key.to_ascii_uppercase()) {
                    entry.value = saved.value.clone();
                    if !saved.comment.trim().is_empty() {
                        entry.comment = saved.comment.clone();
                    }
                }
                entry
            })
            .collect::<Vec<_>>();
        frontend.extend(
            existing
                .into_iter()
                .filter(|entry| !example_keys.contains(&entry.key.to_ascii_uppercase())),
        );
        synchronize_env_entries(&mut frontend, entries);
        let rendered = render_env(&frontend);
        if existing_content.as_deref() != Some(rendered.as_str()) {
            fs::write(&frontend_env_path, rendered)
                .map_err(|error| format!("Failed to write {}: {error}", frontend_env_path.display()))?;
            written.push(frontend_env_path);
        }
    }

    let compose_path = root.join("docker-compose.yml");
    if compose_path.is_file() {
        let compose_content = fs::read_to_string(&compose_path)
            .map_err(|error| format!("Failed to read {}: {error}", compose_path.display()))?;
        let compose_updated = sync_docker_compose_port_mappings(&compose_content, entries);
        let compose_updated = sync_docker_compose_npm_mirror(&compose_updated);
        if compose_updated != compose_content {
            fs::write(&compose_path, &compose_updated)
                .map_err(|error| format!("Failed to write {}: {error}", compose_path.display()))?;
            written.push(compose_path);
        }
    }
    Ok(written)
}

fn synchronize_instance_configs_from_env(
    instance: &InstanceRecord,
) -> Result<Vec<PathBuf>, String> {
    let env_path = instance
        .env_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&instance.work_dir).join(".env"));
    if !env_path.is_file() {
        return Ok(Vec::new());
    }
    let entries = parse_env(
        &fs::read_to_string(&env_path)
            .map_err(|error| format!("Failed to read {}: {error}", env_path.display()))?,
    );
    let root = Path::new(&instance.work_dir);
    let mut written = synchronize_instance_project_name(root, &instance.name)?
        .into_iter()
        .collect::<Vec<_>>();
    for path in synchronize_instance_port_configs(root, &entries)? {
        if !written.contains(&path) {
            written.push(path);
        }
    }
    Ok(written)
}

/// Result of resolving lifecycle ports: rewritten manifest, port change list,
/// and the resolved `(KEY, port)` pairs that must be synced back into .env.
type LifecyclePortPlan = (String, Vec<PortChange>, Vec<(String, u16)>);

fn resolve_lifecycle_ports(
    instance: &InstanceRecord,
    reserved_ports: &HashSet<u16>,
    env_entries: &[EnvVariable],
) -> Result<LifecyclePortPlan, String> {
    let lifecycle_path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    let content = fs::read_to_string(&lifecycle_path)
        .map_err(|error| format!("Failed to read {}: {error}", lifecycle_path.display()))?;
    let manifest: LifecycleManifest = toml::from_str(&content)
        .map_err(|error| format!("Failed to parse {}: {error}", lifecycle_path.display()))?;

    let mut port_map: Vec<(String, u16)> = Vec::new();
    let mut changes = Vec::new();
    let mut taken: HashSet<u16> = reserved_ports.iter().copied().collect();

    for (name, service) in &manifest.services {
        let default_port = extract_url_port(&service.url).unwrap_or(0);
        if default_port == 0 {
            continue;
        }
        let key = format!("{}_PORT", name.to_ascii_uppercase());
        let user_port = env_entries
            .iter()
            .find(|e| e.key.to_ascii_uppercase() == key)
            .and_then(|e| e.value.trim().parse::<u16>().ok())
            .filter(|&p| p > 0);

        if user_port.is_none() {
            let covered = env_entries.iter().any(|e| {
                e.key.to_ascii_uppercase() != key
                    && e.value.trim().parse::<u16>().ok() == Some(default_port)
            });
            if covered {
                continue;
            }
        }

        let preferred = user_port.unwrap_or(default_port);
        let resolved = if port_is_available(preferred) && taken.insert(preferred) {
            preferred
        } else {
            let mut replacement = available_ephemeral_port()?;
            while taken.contains(&replacement) {
                replacement = available_ephemeral_port()?;
            }
            taken.insert(replacement);
            changes.push(PortChange {
                key: key.clone(),
                old_port: preferred,
                new_port: replacement,
            });
            replacement
        };
        port_map.push((key, resolved));
    }

    let mut updated_content = content;
    for (name, service) in &manifest.services {
        let default_port = extract_url_port(&service.url).unwrap_or(0);
        if default_port == 0 {
            continue;
        }
        let resolved = port_map
            .iter()
            .find(|(k, _)| k == &format!("{}_PORT", name.to_ascii_uppercase()))
            .map(|(_, p)| *p)
            .unwrap_or(default_port);
        if resolved != default_port {
            let new_url = replace_url_port(&service.url, resolved);
            updated_content = updated_content.replace(&service.url, &new_url);
        }
    }

    Ok((updated_content, changes, port_map))
}

#[cfg(test)]
mod tests_lifecycle {
    use super::*;

    #[test]
    fn lifecycle_project_name_is_synchronized_without_changing_section_names() {
        let lifecycle = "version = 1\n\
template = \"deepagents/content-builder\"\n\
name = \"Content Builder DeepAgent\" # generated default\n\
[services.frontend]\n\
name = \"Frontend\"\n\
url = \"http://127.0.0.1:5174\"\n";

        let updated = synchronize_lifecycle_project_name_content(lifecycle, "demo2 \\\"draft\\\"");
        let parsed = updated.parse::<toml::Value>().expect("parse lifecycle");

        assert_eq!(
            parsed.get("name").and_then(toml::Value::as_str),
            Some("demo2 \\\"draft\\\"")
        );
        assert!(updated.contains("# generated default"));
        assert!(updated.contains("name = \"Frontend\""));
        assert!(updated.contains("http://127.0.0.1:5174"));
    }
    #[test]
    fn lifecycle_v1_enriches_instance_details() {
        let root = env::temp_dir().join(format!("agentseek-desktop-details-{}", unique_stamp()));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        fs::write(
            metadata.join("lifecycle.toml"),
            "version = 1\nname = \"My Bub Agent\"\n[env.BUB_MODEL]\nrequired = true\n[env.BUB_API_KEY]\nrequired = true\n[services.app]\nurl = \"http://127.0.0.1:5173\"\n[services.gateway]\nurl = \"http://127.0.0.1:8088/agent\"\n[services.copilotkit]\nurl = \"http://127.0.0.1:4000/api/copilotkit\"\n",
        )
        .expect("write lifecycle manifest");
        let env_path = root.join(".env");
        fs::write(
            &env_path,
            "BUB_MODEL=openai:gpt-4o-mini\nBUB_API_KEY=secret-value\nBUB_AG_UI_PORT=55550\nFRONTEND_PORT=55551\nCOPILOTKIT_PORT=57278\n",
        )
        .expect("write env");
        let mut instance = InstanceRecord {
            id: "bub-default".to_string(),
            name: "bub_default".to_string(),
            template_id: "bub/default".to_string(),
            status: "running".to_string(),
            work_dir: root.to_string_lossy().to_string(),
            env_example_path: Some(root.join(".env.example").to_string_lossy().to_string()),
            env_path: Some(env_path.to_string_lossy().to_string()),
            note: String::new(),
            created_at: 1,
            updated_at: 1,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: None,
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };

        // First enrich: None -> value counts as drift, so it reports true (write back).
        assert!(enrich_service_endpoints(&mut instance));

        assert_eq!(instance.project_name.as_deref(), Some("My Bub Agent"));
        assert_eq!(instance.lifecycle_version, Some(1));
        assert_eq!(instance.service_endpoints.len(), 3);
        assert!(instance
            .service_endpoints
            .iter()
            .any(|endpoint| endpoint.primary && endpoint.kind == "web"));
        assert!(instance
            .service_endpoints
            .iter()
            .any(|endpoint| endpoint.kind == "protocol"));
        assert_eq!(instance.ui_url.as_deref(), Some("http://127.0.0.1:55551"));
        assert_eq!(
            instance.agent_url.as_deref(),
            Some("http://127.0.0.1:55550/agent")
        );
        assert!(instance
            .service_endpoints
            .iter()
            .any(|endpoint| endpoint.url == "http://127.0.0.1:57278/api/copilotkit"));

        // Tampered project_name: re-calibrated from the manifest, drift detected.
        instance.project_name = Some("lag-development".to_string());
        assert!(enrich_service_endpoints(&mut instance));
        assert_eq!(instance.project_name.as_deref(), Some("My Bub Agent"));
        // No drift on a subsequent run: nothing to write back.
        assert!(!enrich_service_endpoints(&mut instance));
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn unfinished_instances_are_not_marked_for_restart() {
        let mut store = AppStore {
            instances: vec![InstanceRecord {
                id: "pending-instance".to_string(),
                name: "Pending".to_string(),
                template_id: "deepagents/research".to_string(),
                status: "needs-restart".to_string(),
                work_dir: "/tmp/pending-instance".to_string(),
                env_example_path: Some("/tmp/pending-instance/.env.example".to_string()),
                env_path: Some("/tmp/pending-instance/.env".to_string()),
                note: String::new(),
                created_at: 1,
                updated_at: 1,
                needs_doctor: true,
                pid: None,
                agent_url: None,
                ui_url: None,
                studio_url: None,
                project_name: None,
                lifecycle_version: None,
                service_endpoints: Vec::new(),
            }],
            vault: Vec::new(),
            logs: Vec::new(),
        };

        assert!(repair_predeployment_restart_statuses(&mut store));
        assert_eq!(store.instances[0].status, "ready-to-install");
        assert!(!store.instances[0].needs_doctor);
    }
    #[test]
    fn process_command_port_inserted_from_lifecycle_url_when_no_env_port() {
        // cli-remote: .env has no LANGGRAPH_PORT; port extracted from lifecycle.toml [services.langgraph] URL
        let lifecycle = "version = 1\n\
[services.langgraph]\nurl = \"http://127.0.0.1:54584\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\"]\n";
        let entries = parse_env("LANGGRAPH_URL=http://127.0.0.1:54584\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(updated.contains("--port\""), "should insert --port, got:\n{updated}");
        assert!(updated.contains("\"54584\""), "should contain port from URL, got:\n{updated}");
    }
    #[test]
    fn process_command_without_port_gets_inserted() {
        // cli-remote: lifecycle.toml has [processes.langgraph] but command has no --port
        let lifecycle = "version = 1\n\
[services.langgraph]\nurl = \"http://127.0.0.1:2024\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\"]\n";
        let entries = parse_env("LANGGRAPH_PORT=54584\nLANGGRAPH_URL=http://127.0.0.1:54584\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(updated.contains("--port\""), "should insert --port, got:\n{updated}");
        assert!(updated.contains("\"54584\""), "should contain new port, got:\n{updated}");
    }
    #[test]
    fn process_command_without_port_inserted_preserved_by_synchronize_lifecycle() {
        // Simulate full path of synchronize_instance_port_configs:
        // synchronize_lifecycle_content + sync_process_command_ports
        let lifecycle = "version = 1\r\n\
[services.langgraph]\r\nurl = \"http://127.0.0.1:2024\"\r\n\
[processes.langgraph]\r\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\"]\r\n";
        let entries = parse_env("LANGGRAPH_PORT=54584\n");
        let updated = synchronize_lifecycle_content(lifecycle, &entries);
        let updated = sync_process_command_ports(&updated, &entries);
        assert!(updated != lifecycle, "should differ from original");
        assert!(updated.contains("--port\""), "should insert --port, got:\n{updated}");
        assert!(updated.contains("\"54584\""), "should contain new port, got:\n{updated}");
    }
    #[test]
    fn command_tokens_parses_array_and_string_forms() {
        let arr = command_tokens("command = [\"npm\", \"run\", \"dev\"]");
        assert_eq!(
            arr,
            vec![
                "npm".to_string(),
                "run".to_string(),
                "dev".to_string()
            ]
        );
        let s = command_tokens("command = \"npm run dev\"");
        assert_eq!(
            s,
            vec![
                "npm".to_string(),
                "run".to_string(),
                "dev".to_string()
            ]
        );
        let empty = command_tokens("command = []");
        assert!(empty.is_empty());
    }
    #[test]
    fn remove_command_port_strips_port_from_array_and_string_forms() {
        let arr = "command = [\"npm\", \"install\", \"--port\", \"61986\"]";
        assert_eq!(
            remove_command_port(arr).as_deref(),
            Some("command = [\"npm\", \"install\"]")
        );
        let s = "command = \"npm install --port 61986\"";
        assert_eq!(
            remove_command_port(s).as_deref(),
            Some("command = \"npm install\"")
        );
        // Returns None when no --port
        assert!(remove_command_port("command = [\"npm\", \"install\"]").is_none());
    }
    #[test]
    fn sync_process_command_ports_skips_install_commands() {
        // npm install should not inject --port even if corresponding *_PORT exists
        let lifecycle = "version = 1\n\
[services.app]\nurl = \"http://127.0.0.1:61986\"\n\
[processes.app]\ncommand = [\"npm\", \"install\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "install command must not get --port, got:\n{updated}"
        );
        assert!(
            updated.contains("\"npm\", \"install\"]"),
            "install command should stay clean, got:\n{updated}"
        );
    }
    #[test]
    fn sync_process_command_ports_cleans_injected_port_from_install_commands() {
        // Install commands erroneously injected with --port by old logic should be cleaned up
        let lifecycle = "version = 1\n\
[processes.app]\ncommand = [\"npm\", \"install\", \"--port\", \"61986\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "stale --port must be removed, got:\n{updated}"
        );
        assert!(
            updated.contains("\"npm\", \"install\"]"),
            "install command should be restored, got:\n{updated}"
        );
    }
    #[test]
    fn sync_process_command_ports_injects_port_into_npm_run_dev() {
        // npm run dev — inject "--", "--port", "<port>" so npm passes
        // --port through to the underlying vite/uvicorn process.
        let lifecycle = "version = 1\n\
[services.app]\nurl = \"http://127.0.0.1:61986\"\n\
[processes.app]\ncommand = [\"npm\", \"run\", \"dev\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            updated.contains("\"--\", \"--port\", \"61986\""),
            "npm run dev should get -- --port 61986, got:\n{updated}"
        );
    }
    #[test]
    fn sync_process_command_ports_migrates_bare_port_to_npm_separator() {
        // npm run dev with old-style --port (no -- separator) should be
        // migrated to the correct "--", "--port", "<port>" format.
        let lifecycle = "version = 1\n\
[processes.frontend]\ncommand = [\"npm\", \"run\", \"dev\", \"--port\", \"61986\"]\n";
        let entries = parse_env("FRONTEND_PORT=61986\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            updated.contains("\"--\", \"--port\", \"61986\""),
            "should have -- separator, got:\n{updated}"
        );
        assert!(
            !updated.contains("\"dev\", \"--port\""),
            "old-style bare --port should be gone, got:\n{updated}"
        );
    }
    #[test]
    fn sync_process_command_ports_skips_docker_compose() {
        // docker compose up does not accept --port (reports unknown flag: --port)
        let lifecycle = "version = 1\n\
[services.seekdb]\nurl = \"http://127.0.0.1:2881\"\n\
[processes.seekdb]\ncommand = [\"docker\", \"compose\", \"up\", \"seekdb\"]\n";
        let entries = parse_env("SEEKDB_PORT=2881\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            !updated.contains("--port"),
            "docker compose must not get --port, got:\n{updated}"
        );
    }
    #[test]
    fn sync_docker_compose_ports_replaces_host_port() {
        // Original port 2881 should be replaced with ${SEEKDB_PORT:-2881}:2881 variable reference
        let compose = "name: my_rag_agent\nservices:\n  seekdb:\n    image: quay.io/oceanbase/seekdb:latest\n    ports:\n      - \"127.0.0.1:2881:2881\"\n    volumes:\n      - ./.seekdb-data:/var/lib/oceanbase\n";
        let entries = parse_env("SEEKDB_PORT=2891\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert!(
            updated.contains("${SEEKDB_PORT:-2881}:2881"),
            "should use variable reference, got:\n{updated}"
        );
        assert!(
            !updated.contains("127.0.0.1:2881:2881"),
            "old hardcoded mapping should be gone, got:\n{updated}"
        );
        // Volume mappings unchanged
        assert!(updated.contains("./.seekdb-data:/var/lib/oceanbase"));
    }
    #[test]
    fn sync_docker_compose_ports_no_change_when_no_env_port() {
        // docker-compose.yml should not be modified when .env has no *_PORT
        let compose = "services:\n  seekdb:\n    ports:\n      - \"127.0.0.1:2881:2881\"\n";
        let entries = parse_env("BACKEND_PORT=2024\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert_eq!(updated, compose);
    }
    #[test]
    fn sync_docker_compose_ports_handles_plain_mapping() {
        // Port mapping "2881:2881" without IP prefix should also be replaced with variable reference
        let compose = "services:\n  seekdb:\n    ports:\n      - \"2881:2881\"\n";
        let entries = parse_env("SEEKDB_PORT=2900\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert!(
            updated.contains("${SEEKDB_PORT:-2881}:2881"),
            "should use variable reference, got:\n{updated}"
        );
    }
    #[test]
    fn sync_docker_compose_ports_skips_unrelated_services() {
        // Services without corresponding *_PORT env variable should not be modified
        let compose = "services:\n  seekdb:\n    ports:\n      - \"127.0.0.1:2881:2881\"\n  redis:\n    ports:\n      - \"127.0.0.1:6379:6379\"\n";
        let entries = parse_env("SEEKDB_PORT=2891\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert!(updated.contains("${SEEKDB_PORT:-2881}:2881"));
        assert!(updated.contains("127.0.0.1:6379:6379"));
    }
    #[test]
    fn sync_docker_compose_ports_skips_volume_mappings() {
        // Volume mappings should not be misidentified as port mappings
        let compose = "services:\n  seekdb:\n    ports:\n      - \"127.0.0.1:2881:2881\"\n    volumes:\n      - ./.seekdb-data:/var/lib/oceanbase\n";
        let entries = parse_env("SEEKDB_PORT=2891\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert!(updated.contains("${SEEKDB_PORT:-2881}:2881"));
        assert!(updated.contains("./.seekdb-data:/var/lib/oceanbase"));
    }
    #[test]
    fn sync_docker_compose_ports_idempotent() {
        // Lines already using ${...} syntax should not be modified again (idempotent)
        let compose = "services:\n  seekdb:\n    ports:\n      - \"127.0.0.1:${SEEKDB_PORT:-2881}:2881\"\n";
        let entries = parse_env("SEEKDB_PORT=2900\n");
        let updated = sync_docker_compose_port_mappings(compose, &entries);
        assert_eq!(updated, compose);
    }
    #[test]
    fn sync_process_command_ports_injects_into_shell_wrapped_commands() {
        // sh -lc wrapped commands: --port must be injected INTO the inner
        // command string, not as a separate array element passed to sh.
        let lifecycle = "version = 1\n\
[services.backend]\nurl = \"http://127.0.0.1:63928\"\n\
[processes.backend]\ncommand = [\"sh\", \"-lc\", \"uv run langgraph dev --no-browser\"]\n";
        let entries = parse_env("BACKEND_PORT=63928\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        // --port should appear INSIDE the shell command string
        assert!(
            updated.contains("langgraph dev --no-browser --port 63928"),
            "--port must be injected into the inner shell command string, got:\n{updated}"
        );
        // --port should NOT be a separate array element
        assert!(
            !updated.contains("\", \"--port\", \"63928\""),
            "--port must not be a separate TOML array element for sh -lc commands, got:\n{updated}"
        );
    }
    #[test]
    fn sync_process_command_ports_no_leak_into_tasks() {
        // [tasks.*] command should not be injected with process --port (Bug 1)
        let lifecycle = "version = 1\n\
[services.langgraph]\nurl = \"http://127.0.0.1:61889\"\n\
[services.frontend]\nurl = \"http://127.0.0.1:61884\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\", \"--port\", \"2024\", \"--no-browser\"]\n\
[processes.frontend]\ncommand = [\"npm\", \"run\", \"dev\"]\n\
[tasks.backend]\ncommand = [\"uv\", \"sync\"]\n\
[tasks.frontend]\ncommand = [\"npm\", \"install\", \"--prefix\", \"frontend\"]\n";
        let entries = parse_env("LANGGRAPH_PORT=61889\nFRONTEND_PORT=61884\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        // langgraph command should have port replaced to 61889
        assert!(
            updated.contains("\"61889\""),
            "langgraph should carry resolved port, got:\n{updated}"
        );
        // tasks should NOT have --port leaked from processes
        assert!(
            !updated.contains("sync\", \"--port\""),
            "uv sync in tasks must not get --port, got:\n{updated}"
        );
        assert!(
            !updated.contains("frontend\", \"--port\""),
            "npm install in tasks must not get --port, got:\n{updated}"
        );
    }
    #[test]
    fn accepts_port_flag_whitelist() {
        // langgraph / vite / uvicorn accept --port
        assert!(accepts_port_flag(&[
            "uv".to_string(),
            "run".to_string(),
            "langgraph".to_string(),
            "dev".to_string()
        ]));
        assert!(accepts_port_flag(&["langgraph".to_string(), "dev".to_string()]));
        assert!(accepts_port_flag(&["vite".to_string()]));
        assert!(accepts_port_flag(&[
            "uvicorn".to_string(),
            "main:app".to_string()
        ]));
        // npm run <script> accepts --port via -- separator
        assert!(accepts_port_flag(&[
            "npm".to_string(),
            "run".to_string(),
            "dev".to_string()
        ]));
        // Others do not accept
        assert!(!accepts_port_flag(&[
            "docker".to_string(),
            "compose".to_string(),
            "up".to_string()
        ]));
        assert!(!accepts_port_flag(&["uv".to_string(), "sync".to_string()]));
        assert!(!accepts_port_flag(&["npm".to_string(), "install".to_string()]));
        // sh -lc wrapped commands containing langgraph/uvicorn SHOULD accept --port
        assert!(accepts_port_flag(&[
            "sh".to_string(),
            "-lc".to_string(),
            "uv run langgraph dev".to_string()
        ]));
        // agentseek-api dev also accepts --port
        assert!(accepts_port_flag(&[
            "uv".to_string(),
            "run".to_string(),
            "agentseek-api".to_string(),
            "dev".to_string()
        ]));
        assert!(accepts_port_flag(&["agentseek-api".to_string(), "dev".to_string()]));
    }
    #[test]
    fn sync_process_command_ports_still_injects_port_for_non_npm_commands() {
        // Ensure non-npm commands (langgraph dev) still get --port injected normally
        let lifecycle = "version = 1\n\
[services.langgraph]\nurl = \"http://127.0.0.1:2024\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"langgraph\", \"dev\"]\n";
        let entries = parse_env("LANGGRAPH_PORT=54584\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            updated.contains("--port"),
            "langgraph command should get --port, got:\n{updated}"
        );
        assert!(
            updated.contains("\"54584\""),
            "langgraph command should carry the port, got:\n{updated}"
        );
    }
    #[test]
    fn sync_process_command_ports_injects_port_for_agentseek_api() {
        // agentseek-api dev must also get --port injected (regression: rubric
        // template uses ["uv", "run", "agentseek-api", "dev"] and was starting
        // on the default port 2024 instead of the resolved port).
        let lifecycle = "version = 2\n\
[services.langgraph]\nurl = \"http://127.0.0.1:60496\"\n\
[processes.langgraph]\ncommand = [\"uv\", \"run\", \"agentseek-api\", \"dev\"]\n";
        let entries = parse_env("LANGGRAPH_PORT=60496\n");
        let updated = sync_process_command_ports(lifecycle, &entries);
        assert!(
            updated.contains("--port"),
            "agentseek-api command should get --port, got:\n{updated}"
        );
        assert!(
            updated.contains("\"60496\""),
            "agentseek-api command should carry the resolved port, got:\n{updated}"
        );
    }
    #[test]
    fn resolve_lifecycle_ports_respects_user_configured_port() {
        let root =
            env::temp_dir().join(format!("agentseek-desktop-port-user-{}", unique_stamp()));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        fs::write(
            metadata.join("lifecycle.toml"),
            "version = 1\n[services.langgraph]\nurl = \"http://127.0.0.1:2024\"\n",
        )
        .expect("write lifecycle");
        let instance = InstanceRecord {
            id: "port-test".to_string(),
            name: "port_test".to_string(),
            template_id: "langchain/test".to_string(),
            status: "installing".to_string(),
            work_dir: root.to_string_lossy().to_string(),
            env_example_path: None,
            env_path: None,
            note: String::new(),
            created_at: 1,
            updated_at: 1,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: None,
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };
        let user_port = available_ephemeral_port().expect("allocate user port");
        let entries = parse_env(&format!("LANGGRAPH_PORT={user_port}\n"));
        let reserved = std::collections::HashSet::new();
        let (_updated, changes, port_map) =
            resolve_lifecycle_ports(&instance, &reserved, &entries).expect("resolve lifecycle ports");
        let resolved = port_map
            .iter()
            .find(|(k, _)| k == "LANGGRAPH_PORT")
            .map(|(_, p)| *p);
        assert_eq!(
            resolved,
            Some(user_port),
            "user-configured available port must be respected"
        );
        assert!(
            changes.iter().all(|c| c.key != "LANGGRAPH_PORT"),
            "no change expected when user port is available"
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn resolve_lifecycle_ports_falls_back_to_default_when_env_absent() {
        let root =
            env::temp_dir().join(format!("agentseek-desktop-port-default-{}", unique_stamp()));
        let metadata = root.join(".agentseek");
        fs::create_dir_all(&metadata).expect("create metadata directory");
        let default_port = available_ephemeral_port().expect("allocate default port");
        fs::write(
            metadata.join("lifecycle.toml"),
            format!(
                "version = 1\n[services.langgraph]\nurl = \"http://127.0.0.1:{default_port}\"\n"
            ),
        )
        .expect("write lifecycle");
        let instance = InstanceRecord {
            id: "port-test".to_string(),
            name: "port_test".to_string(),
            template_id: "langchain/test".to_string(),
            status: "installing".to_string(),
            work_dir: root.to_string_lossy().to_string(),
            env_example_path: None,
            env_path: None,
            note: String::new(),
            created_at: 1,
            updated_at: 1,
            needs_doctor: false,
            pid: None,
            agent_url: None,
            ui_url: None,
            studio_url: None,
            project_name: None,
            lifecycle_version: None,
            service_endpoints: Vec::new(),
        };
        let entries = parse_env("");
        let reserved = std::collections::HashSet::new();
        let (_updated, _changes, port_map) =
            resolve_lifecycle_ports(&instance, &reserved, &entries).expect("resolve lifecycle ports");
        let resolved = port_map
            .iter()
            .find(|(k, _)| k == "LANGGRAPH_PORT")
            .map(|(_, p)| *p);
        assert_eq!(
            resolved,
            Some(default_port),
            "should fall back to lifecycle default when env has no port"
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn synchronize_lifecycle_content_empty_input() {
        let entries = parse_env("");
        let result = synchronize_lifecycle_content("", &entries);
        assert_eq!(result, "");
    }
    #[test]
    fn synchronize_lifecycle_content_missing_services_section() {
        let content = "version = 1\nname = \"test\"\n";
        let entries = parse_env("");
        let result = synchronize_lifecycle_content(content, &entries);
        // Content without services should be returned unchanged (or with minimal changes)
        assert!(result.contains("version = 1"));
        assert!(result.contains("name = \"test\""));
    }
    #[test]
    fn lifecycle_manifest_empty_toml() {
        let manifest: LifecycleManifest = toml::from_str("").expect("parse empty toml");
        assert_eq!(manifest.version, 0);
        assert!(manifest.services.is_empty());
    }
    #[test]
    fn lifecycle_manifest_missing_services_section() {
        let manifest: LifecycleManifest =
            toml::from_str("version = 1\nname = \"test\"\n").expect("parse toml");
        assert_eq!(manifest.version, 1);
        assert!(manifest.services.is_empty());
    }
    #[test]
    fn sync_docker_compose_npm_mirror_injects_registry_and_strips_flag() {
        let compose = "services:\n  frontend:\n    image: node:22-slim\n    environment:\n      FOO: bar\n    command:\n      - sh\n      - -lc\n      - |\n        npm install --no-package-lock\n        npx vite\n";
        let updated = sync_docker_compose_npm_mirror(compose);
        assert!(
            updated.contains("NPM_CONFIG_REGISTRY: \"https://registry.npmmirror.com\""),
            "should inject npm mirror, got:\n{updated}"
        );
        assert!(
            !updated.contains("--no-package-lock"),
            "should strip --no-package-lock, got:\n{updated}"
        );
    }
    #[test]
    fn sync_docker_compose_npm_mirror_idempotent() {
        let compose = "services:\n  frontend:\n    image: node:22-slim\n    environment:\n      NPM_CONFIG_REGISTRY: \"https://registry.npmmirror.com\"\n    command:\n      - npm install\n";
        let updated = sync_docker_compose_npm_mirror(compose);
        assert_eq!(
            updated.matches("NPM_CONFIG_REGISTRY").count(),
            1,
            "should not duplicate mirror entry, got:\n{updated}"
        );
    }
    #[test]
    fn sync_docker_compose_npm_mirror_skips_without_npm_install() {
        let compose = "services:\n  backend:\n    image: python:3.12\n    environment:\n      FOO: bar\n";
        let updated = sync_docker_compose_npm_mirror(compose);
        assert_eq!(updated, compose, "should not modify compose without npm install");
    }
}
