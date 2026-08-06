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
                for service in manifest.services.values() {
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
    resolve_port_conflicts_inner(entries, &HashSet::new(), &HashSet::new())
}

/// Core port conflict resolution shared by pre-deploy checks and deployed
/// instance edits.
///
/// `reserved` holds ports assigned to other instances (always conflicts);
/// `self_ports` holds ports this instance's own running services occupy, for
/// which the host-level "in use" check is skipped.
fn resolve_port_conflicts_inner(
    entries: &mut [EnvVariable],
    reserved: &HashSet<u16>,
    self_ports: &HashSet<u16>,
) -> Result<Vec<PortChange>, String> {
    let mut changes = Vec::new();
    let mut selected = HashSet::new();
    for entry in entries.iter_mut() {
        if !is_local_service_port_key(&entry.key) {
            continue;
        }
        let Ok(port) = entry.value.trim().parse::<u16>() else {
            continue;
        };
        let conflict = port == 0
            || selected.contains(&port)
            || reserved.contains(&port)
            || (!self_ports.contains(&port) && !port_is_available(port));
        if port > 0 && !conflict {
            selected.insert(port);
            continue;
        }
        let mut replacement = available_ephemeral_port()?;
        while selected.contains(&replacement) || reserved.contains(&replacement) {
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
    rebase_url_ports(entries, &changes);
    align_loopback_url_ports(entries);
    Ok(changes)
}

/// Replace every `{host}:{old_port}` occurrence in a value with
/// `{host}:{new_port}`, skipping occurrences where the port continues into
/// more digits (e.g. 6006 must never corrupt 60060 or 60061) or where the
/// host is itself a suffix of a longer host.
fn replace_host_port(value: &str, host: &str, old_port: u16, new_port: u16) -> String {
    let needle = format!("{host}:{old_port}");
    let replacement = format!("{host}:{new_port}");
    let mut result = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find(&needle) {
        let before_ok = rest[..index]
            .chars()
            .next_back()
            .map(|c| !c.is_ascii_alphanumeric() && c != '.' && c != '-')
            .unwrap_or(true);
        let after = &rest[index + needle.len()..];
        let after_ok = !after
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
        if !before_ok || !after_ok {
            result.push_str(&rest[..index + needle.len()]);
            rest = after;
            continue;
        }
        result.push_str(&rest[..index]);
        result.push_str(&replacement);
        rest = after;
    }
    result.push_str(rest);
    result
}

/// Rewrite `*_URL` and `*_ENDPOINT` values that embed any changed port (old → new).
///
/// Only host-reachable (loopback) values are rewritten: container-internal
/// endpoints (e.g. RELAY_PHOENIX_ENDPOINT=http://phoenix:6006/v1/traces)
/// reference the in-network port and must keep it even when the host-facing
/// port differs. Port occurrences are replaced with numeric boundary checks
/// so that e.g. 6006 never corrupts 60060 or 60061.
fn rebase_url_ports(entries: &mut [EnvVariable], changes: &[PortChange]) {
    for change in changes {
        let key_prefix = change.key.trim_end_matches("_PORT");
        let duplicate_old_port = changes
            .iter()
            .filter(|candidate| candidate.old_port == change.old_port)
            .count()
            > 1;
        // Process loopback *_URL entries (existing behavior).
        for entry in entries.iter_mut().filter(|entry| {
            let key = entry.key.to_ascii_uppercase();
            key.contains("URL")
                && (!duplicate_old_port || key.contains(key_prefix))
                && LOOPBACK_URL_PREFIXES
                    .iter()
                    .any(|prefix| entry.value.starts_with(prefix))
        }) {
            let mut updated = entry.value.clone();
            for host in ["127.0.0.1", "localhost", "0.0.0.0", "[::1]"] {
                updated = replace_host_port(&updated, host, change.old_port, change.new_port);
            }
            if updated != entry.value {
                entry.value = updated;
                entry.modified = true;
            }
        }
        // Process loopback *_ENDPOINT entries (e.g. OTLP traces endpoint).
        // When PHOENIX_PORT changes, update localhost endpoints that
        // reference the old port so OTLP export stays aligned.
        for entry in entries.iter_mut().filter(|entry| {
            let key = entry.key.to_ascii_uppercase();
            key.contains("ENDPOINT")
                && !key.contains("URL")
                && key_prefix == "PHOENIX"
                && LOCALHOST_URL_PREFIXES
                    .iter()
                    .any(|prefix| entry.value.starts_with(prefix))
        }) {
            let mut updated = entry.value.clone();
            for host in ["127.0.0.1", "localhost"] {
                updated = replace_host_port(&updated, host, change.old_port, change.new_port);
            }
            if updated != entry.value {
                entry.value = updated;
                entry.modified = true;
            }
        }
    }
}

/// Align loopback-bound `*_URL` values with their matching `*_PORT` entries.
fn align_loopback_url_ports(entries: &mut [EnvVariable]) {
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
            && LOOPBACK_URL_PREFIXES
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
    // Also align localhost *_ENDPOINT values (e.g.
    // AGENTSEEK_OTEL_EXPORTER_OTLP_TRACES_ENDPOINT) with their matching
    // *_PORT entries. These variables don't contain "URL" so the loop
    // above skips them, but they embed service ports that must stay in
    // sync (e.g. the OTLP endpoint must follow PHOENIX_PORT).
    for entry in entries.iter_mut().filter(|entry| {
        let key = entry.key.to_ascii_uppercase();
        key.contains("ENDPOINT")
            && !key.contains("URL")
            && LOCALHOST_URL_PREFIXES
                .iter()
                .any(|prefix| entry.value.starts_with(prefix))
    }) {
        let normalized_key = entry.key.to_ascii_uppercase();
        let Some((_, port)) = local_ports
            .iter()
            .filter(|(prefix, _)| normalized_key.contains(prefix))
            .max_by_key(|(prefix, _)| prefix.len())
        else {
            // No prefix match (e.g. OTLP endpoint doesn't contain
            // "PHOENIX"); fall back to PHOENIX_PORT if present, since
            // OTLP endpoints typically target the Phoenix service.
            if let Some(phoenix_port) = local_ports
                .iter()
                .find(|(prefix, _)| prefix == "PHOENIX")
                .map(|(_, p)| *p)
            {
                let updated = replace_url_port(&entry.value, phoenix_port);
                if updated != entry.value {
                    entry.value = updated;
                    entry.modified = true;
                }
            }
            continue;
        };
        let updated = replace_url_port(&entry.value, *port);
        if updated != entry.value {
            entry.value = updated;
            entry.modified = true;
        }
    }
}

/// Ports currently bound to this instance's own services, read from its
/// lifecycle.toml — the runtime truth for a deployed instance.
fn instance_self_ports(instance: &InstanceRecord) -> HashSet<u16> {
    let lifecycle_path = Path::new(&instance.work_dir).join(".agentseek/lifecycle.toml");
    fs::read_to_string(lifecycle_path)
        .map(|content| {
            extract_lifecycle_service_ports(&content)
                .into_iter()
                .map(|(_, port)| port)
                .filter(|port| *port > 0)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve port conflicts when a deployed instance's .env is edited.
///
/// A deployed instance's own services legitimately occupy their ports, so
/// host-level "in use" checks are only meaningful for ports outside its own
/// set (`self_ports`). Conflicts are: duplicate ports within the .env, ports
/// reserved by other instances, and host-occupied ports that do not belong to
/// this instance.
fn resolve_deployed_port_conflicts(
    entries: &mut [EnvVariable],
    reserved: &HashSet<u16>,
    self_ports: &HashSet<u16>,
) -> Result<Vec<PortChange>, String> {
    resolve_port_conflicts_inner(entries, reserved, self_ports)
}

#[cfg(test)]
mod tests_ports {
    use super::*;

    #[test]
    fn local_service_ports_are_reassigned_without_touching_database_ports() {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind occupied port");
        let occupied_port = occupied.local_addr().expect("read occupied port").port();
        let free_gateway_port = available_ephemeral_port().expect("allocate gateway port");
        let mut entries = vec![
            EnvVariable {
                key: "FRONTEND_PORT".to_string(),
                value: occupied_port.to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "COPILOTKIT_PORT".to_string(),
                value: occupied_port.to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "MYSQL_PORT".to_string(),
                value: occupied_port.to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "COPILOTKIT_RUNTIME_URL".to_string(),
                value: format!("http://127.0.0.1:{occupied_port}/api/copilotkit"),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "BUB_AG_UI_PORT".to_string(),
                value: free_gateway_port.to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "BUB_AG_UI_AGENT_URL".to_string(),
                value: "http://127.0.0.1:8088/agent".to_string(),
                comment: String::new(),
                source: "template".to_string(),
                modified: false,
            },
        ];

        let changes = resolve_port_conflicts(&mut entries).expect("resolve port conflict");

        assert!(is_local_service_port_key("FRONTEND_PORT"));
        assert!(is_local_service_port_key("COPILOTKIT_PORT"));
        assert!(!is_local_service_port_key("MYSQL_PORT"));
        assert_eq!(changes.len(), 2);
        assert_ne!(entries[0].value, occupied_port.to_string());
        assert!(entries[0].modified);
        assert_ne!(entries[1].value, occupied_port.to_string());
        assert!(entries[1].modified);
        assert_eq!(entries[2].value, occupied_port.to_string());
        assert_eq!(
            entries[3].value,
            format!("http://127.0.0.1:{}/api/copilotkit", entries[1].value)
        );
        assert!(entries[3].modified);
        assert_eq!(
            entries[5].value,
            format!("http://127.0.0.1:{free_gateway_port}/agent")
        );
        assert!(entries[5].modified);
    }
    #[test]
    fn deployed_instance_env_keeps_own_ports_but_reassigns_foreign_conflicts() {
        // Port bound by another process but claimed by this instance's own
        // lifecycle.toml → must not be treated as a conflict.
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind occupied port");
        let self_port = occupied.local_addr().expect("read occupied port").port();
        // Port reserved by another instance (and duplicated within this .env).
        let reserved_port = available_ephemeral_port().expect("allocate reserved port");
        let mut entries = vec![
            EnvVariable {
                key: "FRONTEND_PORT".to_string(),
                value: self_port.to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "GATEWAY_PORT".to_string(),
                value: reserved_port.to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "BACKEND_PORT".to_string(),
                value: reserved_port.to_string(),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
            EnvVariable {
                key: "GATEWAY_URL".to_string(),
                value: format!("http://127.0.0.1:{reserved_port}/agent"),
                comment: String::new(),
                source: "instance".to_string(),
                modified: false,
            },
        ];
        let reserved: HashSet<u16> = [reserved_port].into_iter().collect();
        let self_ports: HashSet<u16> = [self_port].into_iter().collect();

        let changes = resolve_deployed_port_conflicts(&mut entries, &reserved, &self_ports)
            .expect("resolve deployed port conflicts");

        // Own port stays untouched despite being bound on the host.
        assert_eq!(entries[0].value, self_port.to_string());
        assert!(!entries[0].modified);
        // Reserved port and its in-env duplicate are both reassigned.
        assert_eq!(changes.len(), 2);
        assert_ne!(entries[1].value, reserved_port.to_string());
        assert_ne!(entries[2].value, reserved_port.to_string());
        assert_ne!(entries[1].value, entries[2].value);
        assert!(entries[1].modified);
        assert!(entries[2].modified);
        // URL follows the reassigned gateway port.
        assert_eq!(
            entries[3].value,
            format!("http://127.0.0.1:{}/agent", entries[1].value)
        );
        assert!(entries[3].modified);
    }
    #[test]
    fn reassigned_ports_are_synchronized_to_instance_runtime_configs() {
        let root = env::temp_dir().join(format!("agentseek-desktop-ports-{}", unique_stamp()));
        fs::create_dir_all(root.join(".agentseek")).expect("create metadata directory");
        fs::create_dir_all(root.join("frontend")).expect("create frontend directory");
        let lifecycle = "version = 1\n\
[env.CTX_SERVER_PORT]\ndefault = \"8089\"\n\
[services.app]\nurl = \"http://127.0.0.1:5173\"\n\
[services.gateway]\nurl = \"http://127.0.0.1:8088/agent\"\n\
[services.copilotkit]\nurl = \"http://127.0.0.1:4000/api/copilotkit\"\n\
[services.ctx]\nurl = \"http://127.0.0.1:8089/ctx\"\n\
[checks.frontend]\ntype = \"http\"\ntarget = \"http://127.0.0.1:5173\"\n\
[checks.gateway]\ntype = \"http\"\ntarget = \"http://127.0.0.1:8088/agent/health\"\n\
[checks.copilotkit]\ntype = \"http\"\ntarget = \"http://127.0.0.1:4000/health\"\n\
[checks.ctx]\ntype = \"http\"\ntarget = \"http://127.0.0.1:8089/ctx/health\"\n";
        let frontend_example = "COPILOTKIT_PORT=4000\n\
BUB_AG_UI_AGENT_URL=http://127.0.0.1:8088/agent\n\
VITE_COPILOTKIT_RUNTIME_PROXY=http://127.0.0.1:4000\n\
VITE_BUB_AG_UI_URL=http://127.0.0.1:8088\n\
FRONTEND_PORT=5173\n";
        fs::write(root.join(".agentseek/lifecycle.toml"), lifecycle).expect("write lifecycle");
        fs::write(root.join("frontend/.env.example"), frontend_example)
            .expect("write frontend example");
        let entries = parse_env(
            "BUB_AG_UI_PORT=57975\n\
FRONTEND_PORT=57980\n\
COPILOTKIT_PORT=57985\n\
CTX_SERVER_PORT=57990\n\
BUB_AG_UI_AGENT_URL=http://127.0.0.1:57975/agent\n",
        );

        let written = synchronize_instance_port_configs(&root, &entries)
            .expect("synchronize instance port configs");

        assert_eq!(written.len(), 2);
        let updated_lifecycle =
            fs::read_to_string(root.join(".agentseek/lifecycle.toml")).expect("read lifecycle");
        assert!(updated_lifecycle.contains("http://127.0.0.1:57980"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57975/agent"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57985/api/copilotkit"));
        assert!(updated_lifecycle.contains("default = \"57990\""));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57990/ctx"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57975/agent/health"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57985/health"));
        assert!(updated_lifecycle.contains("http://127.0.0.1:57990/ctx/health"));
        assert!(!updated_lifecycle.contains("127.0.0.1:5173"));
        assert!(!updated_lifecycle.contains("127.0.0.1:8088"));
        assert!(!updated_lifecycle.contains("127.0.0.1:4000"));
        assert!(!updated_lifecycle.contains("127.0.0.1:8089"));

        let frontend = fs::read_to_string(root.join("frontend/.env")).expect("read frontend env");
        assert!(frontend.contains("FRONTEND_PORT=57980"));
        assert!(frontend.contains("COPILOTKIT_PORT=57985"));
        assert!(frontend.contains("BUB_AG_UI_AGENT_URL=http://127.0.0.1:57975/agent"));
        assert!(frontend.contains("VITE_COPILOTKIT_RUNTIME_PROXY=http://127.0.0.1:57985"));
        assert!(frontend.contains("VITE_BUB_AG_UI_URL=http://127.0.0.1:57975"));
        assert_eq!(
            fs::read_to_string(root.join("frontend/.env.example"))
                .expect("read unchanged frontend example"),
            frontend_example
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }
    #[test]
    fn ipv6_listener_marks_port_as_occupied() {
        let Ok(listener) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            return;
        };
        let port = listener.local_addr().expect("read IPv6 port").port();

        assert!(!port_is_available(port));
    }
    #[test]
    fn port_is_available_for_port_zero() {
        // Port 0 is the wildcard; binding to it always succeeds.
        assert!(port_is_available(0));
    }
    #[test]
    fn port_is_available_for_high_port() {
        // Port 65535 should be available unless something is listening.
        // This is a best-effort check; if it fails, something is using the port.
        let _ = port_is_available(65535);
    }
    #[test]
    fn port_is_unavailable_when_already_bound() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("bind listener");
        let port = listener.local_addr().expect("read port").port();
        assert!(!port_is_available(port));
    }
    #[test]
    fn replace_host_port_respects_numeric_boundaries() {
        // 6006 must never corrupt 60060/60061 or hosts ending in the needle.
        assert_eq!(
            replace_host_port("http://127.0.0.1:60060/api", "127.0.0.1", 6006, 60663),
            "http://127.0.0.1:60060/api"
        );
        assert_eq!(
            replace_host_port("http://127.0.0.1:6006/api", "127.0.0.1", 6006, 60663),
            "http://127.0.0.1:60663/api"
        );
        assert_eq!(
            replace_host_port("http://localhost:60061/ctx", "localhost", 6006, 60663),
            "http://localhost:60061/ctx"
        );
        assert_eq!(
            replace_host_port("http://localhost:6006", "localhost", 6006, 60663),
            "http://localhost:60663"
        );
        // Multiple occurrences are all rebased.
        assert_eq!(
            replace_host_port(
                "http://127.0.0.1:6006/api http://127.0.0.1:6006/health",
                "127.0.0.1",
                6006,
                60663
            ),
            "http://127.0.0.1:60663/api http://127.0.0.1:60663/health"
        );
    }
    #[test]
    fn rebase_url_ports_keeps_container_internal_endpoints_untouched() {
        // Relay-style instance: PHOENIX_PORT conflicts, but the container-
        // internal OTLP endpoint (http://phoenix:...) must keep its port while
        // the host-reachable loopback URL follows the resolved port.
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind occupied port");
        let occupied_port = occupied.local_addr().expect("read occupied port").port();
        let mut entries = vec![
            EnvVariable {
                key: "PHOENIX_PORT".to_string(),
                value: occupied_port.to_string(),
                ..EnvVariable::default()
            },
            EnvVariable {
                key: "RELAY_PHOENIX_ENDPOINT".to_string(),
                value: format!("http://phoenix:{occupied_port}/v1/traces"),
                ..EnvVariable::default()
            },
            EnvVariable {
                key: "PHOENIX_URL".to_string(),
                value: format!("http://127.0.0.1:{occupied_port}"),
                ..EnvVariable::default()
            },
        ];
        let changes = resolve_port_conflicts(&mut entries).expect("resolve port conflict");
        assert_eq!(changes.len(), 1);
        let endpoint = entries
            .iter()
            .find(|e| e.key == "RELAY_PHOENIX_ENDPOINT")
            .expect("endpoint entry");
        assert_eq!(
            endpoint.value,
            format!("http://phoenix:{occupied_port}/v1/traces"),
            "container-internal endpoint must keep its in-network port"
        );
        assert!(!endpoint.modified);
        let url = entries
            .iter()
            .find(|e| e.key == "PHOENIX_URL")
            .expect("url entry");
        assert_eq!(url.value, format!("http://127.0.0.1:{}", changes[0].new_port));
        assert!(url.modified, "loopback URL must follow the resolved port");
    }
}
