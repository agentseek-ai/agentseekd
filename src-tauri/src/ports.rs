// Port management utilities.
//
// Provides port availability checking, ephemeral port allocation,
// conflict resolution, and lifecycle port extraction.

fn is_local_service_port_key(key: &str) -> bool {
    let normalized = key.to_ascii_uppercase();
    if normalized != "PORT" && !normalized.ends_with("_PORT") {
        return false;
    }
    ![
        "MYSQL",
        "SEEKDB",
        "OCEANBASE",
    ]
    .iter()
    .any(|external| normalized.contains(external))
}

fn port_is_available(port: u16) -> bool {
    let timeout = Duration::from_millis(150);
    let ipv4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let ipv6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    if TcpStream::connect_timeout(&ipv4, timeout).is_ok()
        || TcpStream::connect_timeout(&ipv6, timeout).is_ok()
    {
        return false;
    }
    if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
        return false;
    }

    // Node commonly listens on an IPv6 wildcard socket that also serves localhost.
    // Skip the IPv6 target check only on systems where IPv6 loopback is unavailable.
    if TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).is_err() {
        return true;
    }
    TcpListener::bind((Ipv6Addr::LOCALHOST, port)).is_ok()
}

fn available_ephemeral_port() -> Result<u16, String> {
    for _ in 0..64 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| format!("Failed to allocate free port: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("Failed to read free port: {error}"))?
            .port();
        drop(listener);
        if port_is_available(port) {
            return Ok(port);
        }
    }
    Err("Failed to find a free port available for both IPv4 and IPv6".to_string())
}

fn collect_assigned_ports(state: &DesktopState, exclude_instance_id: Option<&str>) -> HashSet<u16> {
    // Collect lifecycle paths under a brief lock, then read lifecycle.toml outside the lock.
    let lifecycle_paths: Vec<PathBuf> = {
        state
            .data
            .lock()
            .ok()
            .map(|data| {
                data.instances
                    .iter()
                    .filter(|i| Some(i.id.as_str()) != exclude_instance_id)
                    .map(|i| Path::new(&i.work_dir).join(".agentseek/lifecycle.toml"))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut ports = HashSet::new();
    for lifecycle_path in &lifecycle_paths {
        if let Ok(content) = fs::read_to_string(lifecycle_path) {
            if let Ok(manifest) = toml::from_str::<LifecycleManifest>(&content) {
                for (_, service) in &manifest.services {
                    if let Some(port) = extract_url_port(&service.url) {
                        if port > 0 {
                            ports.insert(port);
                        }
                    }
                }
            }
        }
    }
    ports
}

fn extract_url_port(url: &str) -> Option<u16> {
    url.split("://").nth(1).and_then(|rest| {
        rest.split('/').next().and_then(|host_port| {
            host_port
                .rsplit(':')
                .next()
                .and_then(|port_str| port_str.parse().ok())
        })
    })
}

fn resolve_port_conflicts(entries: &mut [EnvVariable]) -> Result<Vec<PortChange>, String> {
    let mut changes = Vec::new();
    let mut selected = HashSet::new();
    for entry in entries.iter_mut() {
        if !is_local_service_port_key(&entry.key) {
            continue;
        }
        let Ok(port) = entry.value.trim().parse::<u16>() else {
            continue;
        };
        if port > 0 && port_is_available(port) && selected.insert(port) {
            continue;
        }
        let mut replacement = available_ephemeral_port()?;
        while selected.contains(&replacement) {
            replacement = available_ephemeral_port()?;
        }
        selected.insert(replacement);
        entry.value = replacement.to_string();
        entry.modified = true;
        changes.push(PortChange {
            key: entry.key.clone(),
            old_port: port,
            new_port: replacement,
        });
    }
    for change in &changes {
        let key_prefix = change.key.trim_end_matches("_PORT");
        let duplicate_old_port = changes
            .iter()
            .filter(|candidate| candidate.old_port == change.old_port)
            .count()
            > 1;
        for entry in entries.iter_mut().filter(|entry| {
            let key = entry.key.to_ascii_uppercase();
            key.contains("URL") && (!duplicate_old_port || key.contains(key_prefix))
        }) {
            let mut updated = entry.value.clone();
            for host in ["127.0.0.1", "localhost", "0.0.0.0", "[::1]"] {
                updated = updated.replace(
                    &format!("{host}:{}", change.old_port),
                    &format!("{host}:{}", change.new_port),
                );
            }
            if updated != entry.value {
                entry.value = updated;
                entry.modified = true;
            }
        }
    }
    let local_ports = entries
        .iter()
        .filter(|entry| is_local_service_port_key(&entry.key))
        .filter_map(|entry| {
            entry
                .value
                .trim()
                .parse::<u16>()
                .ok()
                .map(|port| (entry.key.trim_end_matches("_PORT").to_string(), port))
        })
        .collect::<Vec<_>>();
    for entry in entries.iter_mut().filter(|entry| {
        entry.key.to_ascii_uppercase().contains("URL")
            && [
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
    }) {
        let normalized_key = entry.key.to_ascii_uppercase();
        let Some((_, port)) = local_ports
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
    Ok(changes)
}
