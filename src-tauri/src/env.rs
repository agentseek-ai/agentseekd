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

/// Sync `*_URL` env variables with ports resolved in lifecycle.toml so that
/// URLs like `LANGGRAPH_URL` stay in sync even when the `.env` file has no
/// corresponding `*_PORT` variable.
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
    for entry in entries.iter_mut() {
        let normalized_key = entry.key.to_ascii_uppercase();
        if !normalized_key.contains("URL") {
            continue;
        }
        if ![
            "http://127.0.0.1",
            "https://127.0.0.1",
            "http://localhost",
            "https://localhost",
            "http://0.0.0.0",
            "https://0.0.0.0",
            "http://[::1]",
            "https://[::1]",
        ]
        .iter()
        .any(|prefix| entry.value.starts_with(prefix))
        {
            continue;
        }
        let Some((_, port)) = lifecycle_ports
            .iter()
            .filter(|(prefix, _)| normalized_key.contains(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
        else {
            continue;
        };
        let updated = replace_url_port(&entry.value, *port);
        if updated != entry.value {
            entry.value = updated;
            entry.modified = true;
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
        if ![
            "http://127.0.0.1",
            "https://127.0.0.1",
            "http://localhost",
            "https://localhost",
            "http://0.0.0.0",
            "https://0.0.0.0",
            "http://[::1]",
            "https://[::1]",
        ]
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
                if !saved.value.trim().is_empty() {
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
