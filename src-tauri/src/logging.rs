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

fn prune_logs(data: &mut AppStore, runtime_retention_days: u32, now: u64) -> Vec<String> {
    let active_instances = data
        .instances
        .iter()
        .map(|instance| instance.id.as_str())
        .collect::<HashSet<_>>();
    let runtime_cutoff = now
        .saturating_sub(u64::from(runtime_retention_days.max(1)).saturating_mul(SECONDS_PER_DAY));
    let deleted_cutoff = now.saturating_sub(
        u64::from(DELETED_INSTANCE_LOG_RETENTION_DAYS).saturating_mul(SECONDS_PER_DAY),
    );
    let mut removed = Vec::new();
    data.logs.retain(|log| {
        let deleted_instance = log
            .instance_id
            .as_deref()
            .is_some_and(|instance_id| !active_instances.contains(instance_id));
        let keep = if deleted_instance && log.created_at < deleted_cutoff {
            false
        } else if log.category == "runtime" {
            log.created_at >= runtime_cutoff
        } else {
            true
        };
        if !keep {
            removed.push(log.id.clone());
        }
        keep
    });
    if data.logs.len() > MAX_LOG_ENTRIES {
        let remove = data.logs.len() - MAX_LOG_ENTRIES + LOG_CLEANUP_BATCH_SIZE;
        let removable = data
            .logs
            .iter()
            .filter(|log| {
                log.category == "runtime"
                    || log
                        .instance_id
                        .as_deref()
                        .is_some_and(|id| !active_instances.contains(id))
            })
            .take(remove)
            .map(|log| log.id.clone())
            .collect::<HashSet<_>>();
        data.logs.retain(|log| {
            if removable.contains(&log.id) {
                removed.push(log.id.clone());
                false
            } else {
                true
            }
        });
    }
    removed
}

fn runtime_stream_level(line: &str) -> &'static str {
    let lower = line.to_lowercase();
    if lower.contains("cannot connect to the docker daemon")
        || lower.contains("is the docker daemon running")
    {
        "error"
    } else if lower.contains("traceback")
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
) -> Result<(fs::File, fs::File), String> {
    let (log_path, cursor_path) = runtime_log_spool_paths(&state.data_dir, instance_id);
    let parent = log_path
        .parent()
        .ok_or_else(|| "Failed to determine runtime log spool directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("Failed to create runtime log spool directory: {error}"))?;

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let stdout = options
        .open(&log_path)
        .map_err(|error| format!("Failed to create runtime log spool file: {error}"))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| format!("Failed to open runtime log error stream: {error}"))?;
    write_runtime_log_cursor(&cursor_path, &RuntimeLogCursor::default())
        .map_err(|error| format!("Failed to initialize runtime log cursor: {error}"))?;
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
