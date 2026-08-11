// Log management: truncation, pruning, runtime log spools, and compaction.

fn truncate_log_text(mut value: String) -> String {
    if value.len() <= MAX_LOG_TEXT_BYTES {
        return value;
    }
    let mut end = MAX_LOG_TEXT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("\n...[log content truncated]");
    value
}

fn runtime_stream_level(line: &str) -> &'static str {
    let lower = line.to_lowercase();
    if lower.contains("cannot connect to the docker daemon")
        || lower.contains("is the docker daemon running")
        || lower.contains("traceback")
        || lower.contains("exception")
        || lower.contains("error")
        || lower.contains("failed")
        || lower.contains("fatal")
        || lower.contains("panic")
    {
        "error"
    } else if lower.contains("warning") || lower.contains("warn") {
        "warning"
    } else {
        "info"
    }
}

fn runtime_log_spool_paths(data_dir: &Path, instance_id: &str) -> (PathBuf, PathBuf) {
    let safe_id = instance_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let directory = data_dir.join(RUNTIME_LOG_SPOOL_DIRECTORY);
    (
        directory.join(format!("{safe_id}.log")),
        directory.join(format!("{safe_id}.cursor")),
    )
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeLogCursor {
    offset: u64,
    suppress_traceback: bool,
}

fn read_runtime_log_cursor(path: &Path) -> RuntimeLogCursor {
    let Ok(value) = fs::read_to_string(path) else {
        return RuntimeLogCursor::default();
    };
    serde_json::from_str(&value).unwrap_or_else(|_| RuntimeLogCursor {
        offset: value.trim().parse::<u64>().unwrap_or_default(),
        suppress_traceback: false,
    })
}

fn write_runtime_log_cursor(path: &Path, cursor: &RuntimeLogCursor) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    let payload = serde_json::to_vec(cursor).map_err(|error| error.to_string())?;
    file.write_all(&payload).map_err(|error| error.to_string())
}

fn prepare_runtime_log_spool(
    state: &DesktopState,
    instance_id: &str,
    truncate: bool,
) -> Result<(fs::File, fs::File), String> {
    let (log_path, cursor_path) = runtime_log_spool_paths(&state.data_dir, instance_id);
    let parent = log_path
        .parent()
        .ok_or_else(|| "Failed to determine runtime log spool directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("Failed to create runtime log spool directory: {error}"))?;

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if truncate {
        options.truncate(true);
    } else {
        options.append(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    let stdout = options
        .open(&log_path)
        .map_err(|error| format!("Failed to create runtime log spool file: {error}"))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| format!("Failed to open runtime log error stream: {error}"))?;
    if truncate {
        write_runtime_log_cursor(&cursor_path, &RuntimeLogCursor::default())
            .map_err(|error| format!("Failed to initialize runtime log cursor: {error}"))?;
    }
    // In append mode (restart), the existing cursor is preserved so
    // sync_runtime_log_spools only reads newly appended records.
    Ok((stdout, stderr))
}

fn strip_ansi_codes(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            characters.next();
            for control in characters.by_ref() {
                if control.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn runtime_log_field<'a>(value: &'a str, field: &str) -> Option<&'a str> {
    let start = value.find(field)?.saturating_add(field.len());
    value[start..]
        .split_whitespace()
        .next()
        .map(|item| item.trim_matches([',', '}', '\'', '"']))
        .filter(|item| !item.is_empty())
}

fn runtime_exception_summary(value: &str) -> Option<String> {
    let clean = strip_ansi_codes(value);
    if clean.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let trimmed = clean.trim();
    let exception_type = trimmed.split_once(':').map_or(trimmed, |(kind, _)| kind);
    let short_type = exception_type.rsplit('.').next().unwrap_or(exception_type);
    if !short_type.ends_with("Error")
        && !short_type.ends_with("Exception")
        && short_type != "KeyboardInterrupt"
    {
        return None;
    }
    if short_type == "TimeoutError" {
        return Some("Failure reason: TimeoutError (request timeout)".to_string());
    }
    let detail = trimmed
        .split_once(':')
        .map(|(_, detail)| detail.trim())
        .filter(|detail| !detail.is_empty());
    Some(match detail {
        Some(detail) => format!("Failure reason: {short_type}: {detail}"),
        None => format!("Failure reason: {short_type}"),
    })
}

fn compact_runtime_log_record(value: &str, suppress_traceback: &mut bool) -> Option<String> {
    let clean = strip_ansi_codes(value);
    let lower = clean.to_lowercase();
    if lower.contains("tool.call.error") {
        *suppress_traceback = true;
        let name = runtime_log_field(&clean, "name=").unwrap_or("unknown");
        let elapsed = runtime_log_field(&clean, "elapsed_time=");
        return Some(match elapsed {
            Some(elapsed) => format!("Tool call failed: {name} ({elapsed})"),
            None => format!("Tool call failed: {name}"),
        });
    }
    if !*suppress_traceback {
        return Some(value.to_string());
    }
    if lower.contains("traceback (most recent call last)") || clean.trim().is_empty() {
        return None;
    }
    if let Some(summary) = runtime_exception_summary(&clean) {
        *suppress_traceback = false;
        return Some(summary);
    }
    if lower.contains("tool.call.start")
        || lower.contains("loop.step")
        || lower.contains("session.run.")
        || lower.trim_start().starts_with("info:")
    {
        *suppress_traceback = false;
        return Some(value.to_string());
    }
    None
}

fn read_runtime_log_records(
    path: &Path,
    cursor: u64,
    include_partial: bool,
) -> Result<(u64, Vec<(String, u64)>), String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    let start = if cursor <= length { cursor } else { 0 };
    file.seek(SeekFrom::Start(start))
        .map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(file);
    let mut position = start;
    let mut records = Vec::new();

    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if bytes == 0 {
            break;
        }
        let complete = line.ends_with('\n');
        if !complete && !include_partial {
            break;
        }
        position = position.saturating_add(bytes as u64);
        records.push((line.trim_end_matches(['\r', '\n']).to_string(), position));
    }
    Ok((start, records))
}

fn sync_runtime_log_spools(state: &DesktopState) {
    let Ok(_sync) = state.runtime_log_sync.lock() else {
        return;
    };
    if state.ensure_storage_ready().is_err() {
        return;
    }
    let instances = state
        .data
        .lock()
        .map(|data| data.instances.clone())
        .unwrap_or_default();

    for instance in instances {
        let (log_path, cursor_path) = runtime_log_spool_paths(&state.data_dir, &instance.id);
        if !log_path.is_file() {
            continue;
        }
        let mut cursor = read_runtime_log_cursor(&cursor_path);
        let include_partial = instance.pid.is_none_or(|pid| !process_exists(pid));
        let Ok((start, records)) =
            read_runtime_log_records(&log_path, cursor.offset, include_partial)
        else {
            continue;
        };
        if start != cursor.offset {
            cursor.suppress_traceback = false;
        }
        let mut persisted_cursor = start;
        for (message, next_cursor) in records {
            let previous_suppression = cursor.suppress_traceback;
            if let Some(compacted) =
                compact_runtime_log_record(&message, &mut cursor.suppress_traceback)
            {
                if !state.log(
                    Some(&instance.id),
                    &instance.name,
                    "runtime",
                    runtime_stream_level(&message),
                    compacted,
                    None,
                ) {
                    cursor.suppress_traceback = previous_suppression;
                    break;
                }
            }
            persisted_cursor = next_cursor;
        }
        if persisted_cursor != cursor.offset {
            cursor.offset = persisted_cursor;
            let _ = write_runtime_log_cursor(&cursor_path, &cursor);
        }
    }
}

fn read_runtime_log_tail(log_path: &Path, max_lines: usize) -> String {
    let content = match fs::read_to_string(log_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let clean = strip_ansi_codes(&content);
    let lines: Vec<&str> = clean.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn remove_runtime_log_spool(state: &DesktopState, instance_id: &str) {
    let (log_path, cursor_path) = runtime_log_spool_paths(&state.data_dir, instance_id);
    let _ = fs::remove_file(log_path);
    let _ = fs::remove_file(cursor_path);
}

// ---------------------------------------------------------------------------
// Log commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn list_logs(state: State<'_, DesktopState>, query: LogQuery) -> Result<LogPage, String> {
    sync_runtime_log_spools(state.inner());
    let _ = state.flush_pending_logs();
    let stored = state
        .storage
        .lock()
        .map_err(|_| "Storage lock is poisoned".to_string())?
        .query_logs(&query);
    let pending = state.pending_logs();
    match stored {
        Ok(mut page) => {
            if pending.is_empty() {
                return Ok(page);
            }
            let mut entries = page.entries;
            let mut known = entries
                .iter()
                .map(|entry| entry.id.clone())
                .collect::<std::collections::HashSet<_>>();
            entries.extend(pending.into_iter().filter(|entry| {
                let after = query.after_sequence.is_none_or(|sequence| entry.sequence > sequence);
                let before = query.before_sequence.is_none_or(|sequence| entry.sequence < sequence);
                after && before && known.insert(entry.id.clone())
            }));
            if query.after_sequence.is_some() {
                entries.sort_by_key(|entry| entry.sequence);
            } else {
                entries.sort_by_key(|entry| std::cmp::Reverse(entry.sequence));
            }
            let limit = if query.load_all { 100_000 } else { query.limit.clamp(1, 1_000) };
            page.has_more |= entries.len() > limit;
            entries.truncate(limit);
            page.entries = entries;
            Ok(page)
        }
        Err(_error) if !pending.is_empty() => {
            let mut entries = pending
                .into_iter()
                .filter(|entry| {
                    query.after_sequence.is_none_or(|sequence| entry.sequence > sequence)
                        && query.before_sequence.is_none_or(|sequence| entry.sequence < sequence)
                })
                .collect::<Vec<_>>();
            if query.after_sequence.is_some() {
                entries.sort_by_key(|entry| entry.sequence);
            } else {
                entries.sort_by_key(|entry| std::cmp::Reverse(entry.sequence));
            }
            let limit = if query.load_all { 100_000 } else { query.limit.clamp(1, 1_000) };
            let has_more = entries.len() > limit;
            entries.truncate(limit);
            Ok(LogPage {
                entries,
                has_more,
                group_count: 0,
            })
        }
        Err(error) => Err(error),
    }
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

#[cfg(test)]
mod tests_logging {
    use super::*;

    #[test]
    fn runtime_tool_tracebacks_are_compacted_to_error_and_reason() {
        let mut suppress_traceback = false;
        assert_eq!(
            compact_runtime_log_record(
                "2026-07-22 17:52:26.376 | ERROR | bub.tools:wrapped:34 - tool.call.error name=web.fetch elapsed_time=153.81ms",
                &mut suppress_traceback,
            )
            .as_deref(),
            Some("Tool call failed: web.fetch (153.81ms)")
        );
        assert!(suppress_traceback);
        assert_eq!(
            compact_runtime_log_record(
                "Traceback (most recent call last):",
                &mut suppress_traceback
            ),
            None
        );
        assert_eq!(
            compact_runtime_log_record(
                "  File \"/tmp/site-packages/aiohttp/client.py\", line 701, in _request",
                &mut suppress_traceback,
            ),
            None
        );
        assert_eq!(
            compact_runtime_log_record(
                "aiohttp.client_exceptions.ClientResponseError: 403, message='Forbidden'",
                &mut suppress_traceback,
            )
            .as_deref(),
            Some("Failure reason: ClientResponseError: 403, message='Forbidden'")
        );
        assert!(!suppress_traceback);

        let mut timeout_traceback = true;
        assert_eq!(
            compact_runtime_log_record("TimeoutError", &mut timeout_traceback).as_deref(),
            Some("Failure reason: TimeoutError (request timeout)")
        );
        assert!(!timeout_traceback);
    }
    #[test]
    fn runtime_log_spool_resumes_only_after_a_complete_line() {
        let root = env::temp_dir().join(format!("agentseek-runtime-spool-{}", unique_stamp()));
        fs::create_dir_all(&root).expect("create runtime spool test directory");
        let (log_path, cursor_path) = runtime_log_spool_paths(&root, "../instance/demo");
        assert_eq!(log_path.parent(), Some(root.join("runtime-logs").as_path()));
        assert_eq!(cursor_path.parent(), log_path.parent());

        fs::create_dir_all(log_path.parent().expect("runtime log parent"))
            .expect("create runtime log directory");
        fs::write(&log_path, "first\npartial").expect("write initial runtime output");
        let (start, records) =
            read_runtime_log_records(&log_path, 0, false).expect("read complete runtime line");
        assert_eq!(start, 0);
        assert_eq!(records, vec![("first".to_string(), 6)]);

        let mut output = fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("reopen runtime output");
        output
            .write_all(b" tail\n")
            .expect("complete partial runtime line");
        let (_, resumed) =
            read_runtime_log_records(&log_path, 6, false).expect("resume runtime output");
        assert_eq!(
            resumed,
            vec![(
                "partial tail".to_string(),
                "first\npartial tail\n".len() as u64
            )]
        );

        fs::remove_dir_all(root).expect("remove runtime spool test directory");
    }
    #[test]
    fn oversized_log_text_is_truncated_on_a_utf8_boundary() {
        let value = "\u{2192}".repeat(MAX_LOG_TEXT_BYTES);
        let truncated = truncate_log_text(value);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.contains("log content truncated"));
        assert!(truncated.len() < MAX_LOG_TEXT_BYTES + 100);
    }
    #[test]
    fn runtime_stream_level_does_not_treat_normal_stderr_as_an_error() {
        assert_eq!(
            runtime_stream_level("INFO: Application startup complete."),
            "info"
        );
        assert_eq!(runtime_stream_level("WARNING: retrying request"), "warning");
        assert_eq!(
            runtime_stream_level("RuntimeError: connection failed"),
            "error"
        );
        assert_eq!(
            runtime_stream_level("unable to get image 'quay.io/oceanbase/seekdb:latest': Cannot connect to the Docker daemon at unix:///Users/sunchong/.orbstack/run/docker.sock. Is the docker daemon running?"),
            "error"
        );
        assert_eq!(
            runtime_stream_level("Cannot connect to the Docker daemon at unix:///Users/sunchong/.orbstack/run/docker.sock. Is the docker daemon running?"),
            "error"
        );
    }
}

#[cfg(test)]
fn test_log(id: &str, instance_id: Option<&str>, category: &str, created_at: u64) -> LogEntry {
    LogEntry {
        id: id.to_string(),
        instance_id: instance_id.map(str::to_string),
        instance_name: instance_id.unwrap_or("AgentSeek").to_string(),
        category: category.to_string(),
        level: "info".to_string(),
        message: id.to_string(),
        command: None,
        created_at,
        sequence: created_at,
    }
}
