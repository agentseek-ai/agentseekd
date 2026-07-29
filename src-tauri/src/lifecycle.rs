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

fn enrich_service_endpoints(instance: &mut InstanceRecord) {
    let path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    let Some(manifest) = fs::read_to_string(path)
        .ok()
        .and_then(|content| toml::from_str::<LifecycleManifest>(&content).ok())
    else {
        return;
    };
    if instance.project_name.is_none() && !manifest.name.trim().is_empty() {
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
        .map(|(name, service)| {
            let url = service_env_port(&name, &env)
                .map(|port| replace_url_port(&service.url, port))
                .unwrap_or(service.url);
            (name, url)
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
        .map(|(name, url)| {
            let (kind, primary) = service_kind(name);
            ServiceEndpoint {
                name: service_display_name(name),
                url: url.clone(),
                kind: kind.to_string(),
                primary,
            }
        })
        .collect();
    for (name, url) in services {
        match name.to_ascii_lowercase().as_str() {
            "gateway" | "agent" => instance.agent_url = Some(url),
            "app" | "frontend" | "web" => instance.ui_url = Some(url),
            "studio" | "langsmith" => instance.studio_url = Some(url),
            _ => {}
        }
    }
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

fn replace_project_name_in_directory(
    dir: &Path,
    old_name: &str,
    new_name: &str,
) -> Result<(), String> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = fs::read_dir(&current)
            .map_err(|error| format!("Failed to read directory {}: {error}", current.display()))?;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                if !path.to_string_lossy().contains("/.git") {
                    stack.push(path);
                }
            } else if path.is_file() {
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };
                if content.contains(old_name) {
                    let updated = content.replace(old_name, new_name);
                    fs::write(&path, updated)
                        .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
                }
            }
        }
    }
    Ok(())
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
            || lower.contains("langgraph") || lower.contains("uvicorn")
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

fn resolve_lifecycle_ports(
    instance: &InstanceRecord,
    reserved_ports: &HashSet<u16>,
    env_entries: &[EnvVariable],
) -> Result<(String, Vec<PortChange>, Vec<(String, u16)>), String> {
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
