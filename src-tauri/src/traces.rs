// Trace domain: ATOF (Agent Trace Output Format) parsing.
//
// Reads `.nemo-relay/atof/events.jsonl`, groups OpenInference/OTEL spans
// into traces, and builds span trees for the detail view.
//
// NOTE: this file is include!()-ed into lib.rs; all `use` items are
// inherited from the parent module. Data types live in models.rs.

// ---------------------------------------------------------------------------
// ATOF parsing helpers
// ---------------------------------------------------------------------------

/// Maximum length (in characters) for trace/span summary strings.
const SUMMARY_TRUNCATE_LEN: usize = 120;

fn atof_path(work_dir: &str) -> PathBuf {
    Path::new(work_dir).join(".nemo-relay/atof/events.jsonl")
}

/// Return a field from a JSON value, trying several possible keys (snake/camel).
fn json_str(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(v) = value.get(key).and_then(|v| v.as_str()) {
            let s = v.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Try to extract a summary string (first SUMMARY_TRUNCATE_LEN chars) from a JSON value.
fn summary(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(truncate_chars(s, SUMMARY_TRUNCATE_LEN))
            }
        }
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            let s = serde_json::to_string(value).unwrap_or_default();
            Some(truncate_chars(&s, SUMMARY_TRUNCATE_LEN))
        }
        _ => None,
    }
}

/// Normalize span kinds to the labels Phoenix exposes in its tables.
/// Relay payloads can use lowercase values or the OpenInference enum prefix.
fn normalize_span_kind(value: &str) -> String {
    let normalized = value.trim().to_ascii_uppercase();
    let normalized = normalized
        .strip_prefix("SPANKIND.")
        .or_else(|| normalized.strip_prefix("SPAN_KIND."))
        .unwrap_or(&normalized);
    if normalized.is_empty() {
        "UNKNOWN".to_string()
    } else {
        normalized.to_string()
    }
}

/// Phoenix reports Relay's root `model` scope as an AGENT span. Keep that
/// root-level convention while preserving LLM kinds for nested model calls.
fn phoenix_root_kind(name: &str, kind: &str) -> String {
    let name = name.trim().to_ascii_lowercase();
    if name == "model" && matches!(kind, "LLM" | "MODEL") {
        "AGENT".to_string()
    } else {
        kind.to_string()
    }
}

/// Parse ISO-8601 timestamp (e.g. "2026-08-04T06:30:58.123Z") or a numeric
/// unix timestamp (seconds or millis) into millis since epoch.
///
/// Note: ISO timestamps with offsets ("+08:00") are treated as UTC; the
/// offset is ignored. This is a best-effort parse for relative durations,
/// not a full timezone-aware conversion.
fn parse_iso_millis(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Numeric unix timestamp: seconds (< 1e12) or millis (>= 1e12).
    if let Ok(n) = s.parse::<f64>() {
        if n > 1e12 {
            return Some(n as u64);
        }
        return Some((n * 1000.0) as u64);
    }
    // ISO-8601: "YYYY-MM-DDTHH:MM:SS[.fff][Z|+HH:MM]" (date part required).
    let (date_part, time_part) = s.split_once('T').or_else(|| s.split_once('t'))?;
    let mut date = date_part.split('-');
    let year = date.next()?.parse::<u64>().ok()?;
    let month = date.next()?.parse::<u32>().ok()?;
    let day = date.next()?.parse::<u32>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Separate the clock, fractional seconds, and timezone offset.
    let raw_time = time_part
        .trim_end_matches('Z')
        .trim_end_matches('z');
    let offset_index = raw_time[1..]
        .find('+')
        .or_else(|| raw_time[1..].find('-'))
        .map(|index| index + 1);
    let (time, timezone_offset_seconds) = if let Some(index) = offset_index {
        let offset = &raw_time[index..];
        let sign = if offset.starts_with('-') { -1i64 } else { 1i64 };
        let mut parts = offset[1..].split(':');
        let hours = parts.next()?.parse::<i64>().ok()?;
        let minutes = parts.next().unwrap_or("0").parse::<i64>().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        (&raw_time[..index], sign * (hours * 3_600 + minutes * 60))
    } else {
        (raw_time, 0)
    };
    let mut hms = time.split(':');
    let hour = hms.next()?.parse::<u32>().ok()?;
    let minute = hms.next().unwrap_or("0").parse::<u32>().ok()?;
    let second_part = hms.next().unwrap_or("0");
    let (second_value, fraction) = second_part.split_once('.').unwrap_or((second_part, ""));
    let second = second_value.parse::<u32>().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let fraction_digits: String = fraction.chars().take(3).collect();
    let fraction_ms = if fraction_digits.is_empty() {
        0i64
    } else {
        let value = fraction_digits.parse::<i64>().ok()?;
        value * 10i64.pow(3 - fraction_digits.len() as u32)
    };
    // Days-from-civil algorithm (Howard Hinnant) for proleptic Gregorian dates.
    let days_from_civil = |y: i64, m: i64, d: i64| -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    };
    let days = days_from_civil(year as i64, month as i64, day as i64);
    let millis = days
        .saturating_mul(86_400)
        .saturating_add(i64::from(hour) * 3_600)
        .saturating_add(i64::from(minute) * 60)
        .saturating_add(i64::from(second))
        .saturating_mul(1_000)
        .saturating_add(fraction_ms)
        .saturating_sub(timezone_offset_seconds.saturating_mul(1_000));
    u64::try_from(millis).ok()
}

fn duration_ms(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    let start_ms = parse_iso_millis(start?)?;
    let end_ms = parse_iso_millis(end?)?;
    if end_ms > start_ms {
        Some(end_ms - start_ms)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Raw event → flat span record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RawSpan {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    name: String,
    kind: String,
    status: String,
    start_time: Option<String>,
    end_time: Option<String>,
    input: Option<serde_json::Value>,
    output: Option<serde_json::Value>,
    attributes: Option<serde_json::Value>,
}

fn parse_raw_span(event: &serde_json::Value) -> Option<RawSpan> {
    let trace_id = json_str(event, &["trace_id", "traceId", "context.trace_id"])?;
    let span_id = json_str(event, &["span_id", "spanId", "context.span_id"])?;
    let name = json_str(event, &["name", "span_name", "spanName"])?;
    let attributes = event.get("attributes");
    let kind = json_str(event, &["kind", "span_kind", "spanKind"])
        .or_else(|| {
            attributes.and_then(|attrs| {
                json_str(attrs, &["openinference.span.kind", "span.kind", "spanKind", "kind"])
            })
        })
        .map(|value| normalize_span_kind(&value))
        .unwrap_or_else(|| "UNKNOWN".to_string());
    // Status: look for status.code or status.message
    let status = event
        .get("status")
        .and_then(|s| s.get("code"))
        .and_then(|c| c.as_str())
        .or_else(|| event.get("status_code").and_then(|v| v.as_str()))
        .map(|s| {
            if s == "OK" || s == "0" || s == "STATUS_CODE_OK" {
                "OK".to_string()
            } else {
                "ERROR".to_string()
            }
        })
        .unwrap_or_else(|| {
            if event.get("error").is_some()
                || event.get("exception").is_some()
            {
                "ERROR".to_string()
            } else {
                "OK".to_string()
            }
        });

    let parent_span_id =
        json_str(event, &["parent_span_id", "parentSpanId", "parent_id", "parentId"]);

    let start_time = json_str(event, &["start_time", "startTime", "timestamp"]);
    let end_time = json_str(event, &["end_time", "endTime"]);

    // Attributes often contain input/output inside an "attributes" object
    let attrs = attributes;
    let input = attrs
        .and_then(|a| a.get("input"))
        .or_else(|| event.get("input"))
        .or_else(|| attrs.and_then(|a| a.get("input.value")))
        .or_else(|| attrs.and_then(|a| a.get("llm.input_messages")))
        .cloned();
    let output = attrs
        .and_then(|a| a.get("output"))
        .or_else(|| event.get("output"))
        .or_else(|| attrs.and_then(|a| a.get("output.value")))
        .or_else(|| attrs.and_then(|a| a.get("llm.output_messages")))
        .cloned();
    let attributes = attrs.cloned();

    Some(RawSpan {
        trace_id,
        span_id,
        parent_span_id,
        name,
        kind,
        status,
        start_time,
        end_time,
        input,
        output,
        attributes,
    })
}

// ---------------------------------------------------------------------------
// Standard ATOF event parsing (Nemo Relay generic event stream)
// ---------------------------------------------------------------------------

/// One half (start or end) of a standard ATOF scope event.
#[derive(Debug, Clone)]
struct AtofEvent {
    uuid: String,
    parent_uuid: Option<String>,
    name: String,
    category: String,
    scope_category: Option<String>,
    timestamp: Option<String>,
    data: Option<serde_json::Value>,
}

fn parse_atof_event(event: &serde_json::Value) -> Option<AtofEvent> {
    let uuid = json_str(event, &["uuid"])?;
    let name = json_str(event, &["name"]).unwrap_or_default();
    let category =
        json_str(event, &["category"]).unwrap_or_else(|| "event".to_string());
    let parent_uuid = json_str(event, &["parent_uuid"]).filter(|s| !s.is_empty());
    let scope_category = json_str(event, &["scope_category"]);
    let timestamp = json_str(event, &["timestamp"]);
    let data = event.get("data").cloned();
    Some(AtofEvent {
        uuid,
        parent_uuid,
        name,
        category,
        scope_category,
        timestamp,
        data,
    })
}

/// Parse a standard ATOF event stream into flat span records.
///
/// Each scope is represented by a start/end event pair sharing a `uuid`;
/// scopes link via `parent_uuid`. Each root scope is a separate trace, and
/// its UUID is inherited by descendants. This matches Phoenix's root-span
/// view instead of merging multiple root scopes from one session/chat.
fn parse_atof_spans(content: &str) -> Vec<RawSpan> {
    struct Paired {
        start: Option<AtofEvent>,
        end: Option<AtofEvent>,
    }

    let mut by_uuid: HashMap<String, Paired> = HashMap::new();
    let mut uuid_order: HashMap<String, usize> = HashMap::new();
    for (line_number, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(ev) = parse_atof_event(&event) else {
            continue;
        };
        uuid_order.entry(ev.uuid.clone()).or_insert(line_number);
        let is_end = ev.scope_category.as_deref() == Some("end");
        let pair = by_uuid.entry(ev.uuid.clone()).or_insert(Paired {
            start: None,
            end: None,
        });
        if is_end {
            pair.end = Some(ev);
        } else {
            pair.start = Some(ev);
        }
    }

    let uuids: HashSet<String> = by_uuid.keys().cloned().collect();
    // Root events: parent missing from the file (or absent entirely).
    let root_uuids: HashSet<String> = by_uuid
        .iter()
        .filter(|(_, pair)| {
            pair.start
                .as_ref()
                .or(pair.end.as_ref())
                .and_then(|e| e.parent_uuid.as_deref())
                .is_none_or(|parent| !uuids.contains(parent))
        })
        .map(|(uuid, _)| uuid.clone())
        .collect();

    // Every root scope is shown as its own trace, even when roots share
    // the same session_id or chat_id.
    let mut trace_id_of: HashMap<String, String> = HashMap::new();
    for root in &root_uuids {
        trace_id_of.insert(root.clone(), root.clone());
    }

    let mut spans: Vec<RawSpan> = Vec::new();
    for (uuid, pair) in &by_uuid {
        let Some(first) = pair.start.as_ref().or(pair.end.as_ref()) else {
            continue;
        };
        // Walk up the parent chain (memoized) to find the trace root.
        let mut trace_id: Option<String> = None;
        let mut current = Some(uuid.clone());
        let mut hops = 0;
        while let Some(id) = current {
            if let Some(tid) = trace_id_of.get(&id) {
                trace_id = Some(tid.clone());
                break;
            }
            current = by_uuid
                .get(&id)
                .and_then(|p| p.start.as_ref().or(p.end.as_ref()))
                .and_then(|e| e.parent_uuid.clone());
            hops += 1;
            if hops > 1024 {
                break;
            }
        }
        let trace_id = trace_id.unwrap_or_else(|| uuid.clone());
        trace_id_of.insert(uuid.clone(), trace_id.clone());

        let end = pair.end.as_ref();
        let status = match end.and_then(|e| e.data.as_ref()) {
            Some(d) if d.get("error").is_some() || d.get("exception").is_some() => "ERROR",
            _ => "OK",
        };
        spans.push(RawSpan {
            trace_id,
            span_id: uuid.clone(),
            parent_span_id: first
                .parent_uuid
                .clone()
                .filter(|parent| uuids.contains(parent)),
            name: first.name.clone(),
            kind: normalize_span_kind(&first.category),
            status: status.to_string(),
            start_time: first.timestamp.clone(),
            end_time: end.and_then(|e| e.timestamp.clone()),
            input: pair.start.as_ref().and_then(|e| e.data.clone()),
            output: end.and_then(|e| e.data.clone()),
            attributes: None,
        });
    }
    spans.sort_by_key(|span| {
        uuid_order
            .get(&span.span_id)
            .copied()
            .unwrap_or(usize::MAX)
    });
    spans
}

// ---------------------------------------------------------------------------
// Span tree building
// ---------------------------------------------------------------------------

fn build_span_tree(flat: &[RawSpan]) -> Vec<SpanNode> {
    // Keep all nodes in one map while building the parent-child index. Nodes
    // must not be removed before their own children are attached.
    let mut node_map: HashMap<String, SpanNode> = HashMap::new();
    for span in flat {
        node_map.insert(
            span.span_id.clone(),
            SpanNode {
                span_id: span.span_id.clone(),
                name: span.name.clone(),
                kind: span.kind.clone(),
                status: span.status.clone(),
                start_time: span.start_time.clone(),
                end_time: span.end_time.clone(),
                duration_ms: duration_ms(
                    span.start_time.as_deref(),
                    span.end_time.as_deref(),
                ),
                input: span.input.clone(),
                output: span.output.clone(),
                attributes: span.attributes.clone(),
                children: Vec::new(),
            },
        );
    }

    let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
    let mut is_child: HashSet<String> = HashSet::new();
    for span in flat {
        if let Some(ref parent_id) = span.parent_span_id {
            if node_map.contains_key(parent_id) {
                is_child.insert(span.span_id.clone());
                children_by_parent
                    .entry(parent_id.clone())
                    .or_default()
                    .push(span.span_id.clone());
            }
        }
    }

    fn build_node(
        id: &str,
        node_map: &HashMap<String, SpanNode>,
        children_by_parent: &HashMap<String, Vec<String>>,
        visiting: &mut HashSet<String>,
    ) -> Option<SpanNode> {
        if !visiting.insert(id.to_string()) {
            return None;
        }

        let mut node = node_map.get(id)?.clone();
        let child_ids = children_by_parent.get(id).cloned().unwrap_or_default();
        node.children = child_ids
            .iter()
            .filter_map(|child_id| build_node(child_id, node_map, children_by_parent, visiting))
            .collect();
        visiting.remove(id);
        Some(node)
    }

    let root_ids: Vec<String> = flat
        .iter()
        .filter(|span| !is_child.contains(&span.span_id))
        .map(|span| span.span_id.clone())
        .collect();
    let mut visiting = HashSet::new();
    let mut roots: Vec<SpanNode> = root_ids
        .iter()
        .filter_map(|id| build_node(id, &node_map, &children_by_parent, &mut visiting))
        .collect();

    // Preserve the source order supplied by ATOF (or Phoenix's fallback
    // response). The order is established while collecting flat spans.
    fn sort_children(node: &mut SpanNode) {
        for child in &mut node.children {
            sort_children(child);
        }
    }
    for root in &mut roots {
        sort_children(root);
    }

    roots
}

// ---------------------------------------------------------------------------
// ATOF public API
// ---------------------------------------------------------------------------

/// Parse the ATOF archive and return paginated trace summaries for the list page.
/// Load and parse all raw spans from the ATOF events file.
/// Returns an empty Vec if the file does not exist or contains no recognized spans.
fn load_atof_raw_spans(work_dir: &str) -> Result<Vec<RawSpan>, String> {
    let path = atof_path(work_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(&path).map_err(|e| format!("Failed to read ATOF: {e}"))?;

    let mut raw_spans: Vec<RawSpan> = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("Invalid ATOF JSONL: {e}"))?;
        if let Some(span) = parse_raw_span(&event) {
            raw_spans.push(span);
        }
    }
    // Fall back to the standard ATOF event format (uuid/parent_uuid scopes)
    // when no OpenInference spans were recognized.
    if raw_spans.is_empty() {
        raw_spans = parse_atof_spans(&content);
    }
    Ok(raw_spans)
}

pub(crate) fn parse_trace_summaries(
    work_dir: &str,
    page: usize,
    page_size: usize,
) -> Result<TracePage, String> {
    let raw_spans = load_atof_raw_spans(work_dir)?;
    if raw_spans.is_empty() {
        return Ok(TracePage { entries: Vec::new(), total: 0, page, page_size });
    }

    // Group by trace_id
    let mut trace_map: HashMap<String, Vec<RawSpan>> = HashMap::new();
    for span in raw_spans {
        trace_map
            .entry(span.trace_id.clone())
            .or_default()
            .push(span);
    }

    let mut summaries: Vec<TraceSummary> = Vec::new();
    for (trace_id, spans) in &trace_map {
        // Root span = span without parent, or first span
        let root = spans
            .iter()
            .find(|s| s.parent_span_id.is_none() || !spans.iter().any(|other| other.span_id == s.parent_span_id.clone().unwrap_or_default()))
            .or_else(|| spans.first());

        let status = root.map(|r| r.status.clone()).unwrap_or_else(|| "OK".to_string());
        let name = root.map(|r| r.name.clone()).unwrap_or_default();
        let kind = root
            .map(|r| phoenix_root_kind(&r.name, &r.kind))
            .unwrap_or_default();

        let input_summary = root.and_then(|r| r.input.as_ref().and_then(summary));
        let output_summary = root.and_then(|r| r.output.as_ref().and_then(summary));

        let start_time = root.and_then(|r| r.start_time.clone());

        let latency_ms = root
            .and_then(|r| duration_ms(r.start_time.as_deref(), r.end_time.as_deref()));

        summaries.push(TraceSummary {
            trace_id: trace_id.clone(),
            status,
            kind,
            name,
            input_summary,
            output_summary,
            start_time,
            latency_ms,
            span_count: spans.len(),
        });
    }

    // Newest first
    summaries.sort_by(|a, b| b.start_time.cmp(&a.start_time));

    let total = summaries.len();
    let page = if page < 1 { 1 } else { page };
    let page_size = if page_size < 1 { 20 } else { page_size };
    let start = (page - 1) * page_size;
    let entries = if start < total {
        summaries.into_iter().skip(start).take(page_size).collect()
    } else {
        Vec::new()
    };

    Ok(TracePage { entries, total, page, page_size })
}

/// Parse the ATOF archive and return a single trace with full span tree.
pub(crate) fn parse_trace_detail(
    work_dir: &str,
    trace_id: &str,
) -> Result<Option<TraceDetail>, String> {
    let all_spans = load_atof_raw_spans(work_dir)?;
    let matching: Vec<RawSpan> = all_spans
        .into_iter()
        .filter(|span| span.trace_id == trace_id)
        .collect();

    if matching.is_empty() {
        return Ok(None);
    }

    let root = matching
        .iter()
        .find(|s| s.parent_span_id.is_none())
        .or_else(|| matching.first());

    let status = root.map(|r| r.status.clone()).unwrap_or_else(|| "OK".to_string());
    let start_time = root.and_then(|r| r.start_time.clone());
    let latency_ms = root.and_then(|r| {
        duration_ms(r.start_time.as_deref(), r.end_time.as_deref())
    });

    let spans = build_span_tree(&matching);

    let mut spans = spans;
    for root in &mut spans {
        root.kind = phoenix_root_kind(&root.name, &root.kind);
    }

    Ok(Some(TraceDetail {
        trace_id: trace_id.to_string(),
        status,
        latency_ms,
        start_time,
        spans,
    }))
}

// ---------------------------------------------------------------------------
// Phoenix GraphQL fallback
// ---------------------------------------------------------------------------

/// Escape a string value for safe interpolation inside a GraphQL string literal.
/// Handles `\`, `"`, and line terminators (which are not allowed unescaped in
/// GraphQL string literals) so the query stays syntactically valid.
fn graphql_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Execute a GraphQL query against a Phoenix endpoint via `curl`.
fn phoenix_graphql(phoenix_url: &str, query: &str) -> Result<serde_json::Value, String> {
    let body = serde_json::json!({ "query": query });
    let body_str = serde_json::to_string(&body).map_err(|e| format!("JSON serialize: {e}"))?;
    let url = format!("{}/graphql", phoenix_url.trim_end_matches('/'));
    let output = Command::new("curl")
        .args(["-sS", "-X", "POST", &url, "-H", "Content-Type: application/json", "-d", &body_str, "--max-time", "10"])
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Phoenix GraphQL request failed: {}", stderr.trim()));
    }
    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("Invalid Phoenix response: {e}"))?;
    if let Some(errors) = response.get("errors") {
        return Err(format!("Phoenix GraphQL errors: {errors}"));
    }
    Ok(response)
}

/// Discover the Phoenix project name for this instance.
///
/// Tries the instance `service_name` first, then falls back to scanning all
/// Phoenix projects and picking the first one with traces.
fn discover_phoenix_project(phoenix_url: &str, service_name: Option<&str>) -> Result<Option<String>, String> {
    if let Some(name) = service_name {
        let name = graphql_escape(name);
        let q = format!(
            r#"{{ getProjectByName(name: "{name}") {{ name traceCount }} }}"#
        );
        if let Ok(resp) = phoenix_graphql(phoenix_url, &q) {
            if resp.get("data").and_then(|d| d.get("getProjectByName")).is_some() {
                return Ok(Some(name.to_string()));
            }
        }
    }
    let resp = phoenix_graphql(
        phoenix_url,
        "{ projects(first: 20) { edges { node { name traceCount } } } }",
    )?;
    let projects = resp
        .pointer("/data/projects/edges")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    projects
        .into_iter()
        .filter_map(|e| {
            let node = e.get("node")?;
            let count = node.get("traceCount").and_then(|v| v.as_u64()).unwrap_or(0);
            let name = node.get("name").and_then(|v| v.as_str())?;
            if count > 0 { Some(name.to_string()) } else { None }
        })
        .next()
        .map_or_else(|| Ok(None), |n| Ok(Some(n)))
}

/// Query Phoenix GraphQL for paginated trace summaries.
fn query_phoenix_trace_summaries(
    phoenix_url: &str,
    project_name: &str,
    page: usize,
    page_size: usize,
) -> Result<TracePage, String> {
    let page = if page < 1 { 1 } else { page };
    let page_size = if page_size < 1 { 20 } else { page_size };

    let project_name = graphql_escape(project_name);

    // First get total trace count
    let count_q = format!(
        r#"{{ getProjectByName(name: "{project_name}") {{ traceCount }} }}"#
    );
    let count_resp = phoenix_graphql(phoenix_url, &count_q)?;
    let total = count_resp
        .pointer("/data/getProjectByName/traceCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    if total == 0 {
        return Ok(TracePage { entries: Vec::new(), total: 0, page, page_size });
    }

    // Calculate cursor-based pagination offset
    let skip = (page - 1) * page_size;
    let fetch_count = page_size;

    let query = format!(
        r#"{{ getProjectByName(name: "{project_name}") {{ spans(first: {fetch_count}, rootSpansOnly: true, sort: {{col: startTime, dir: desc}}) {{ edges {{ node {{ spanId name statusCode spanKind startTime endTime latencyMs trace {{ traceId numSpans }} input {{ value truncatedValue }} output {{ value truncatedValue }} }} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"#
    );

    let mut resp = phoenix_graphql(phoenix_url, &query)?;

    // If we need to skip past earlier pages, paginate through cursors
    if skip > 0 {
        let mut remaining = skip;
        while remaining > 0 {
            let has_next = resp
                .pointer("/data/getProjectByName/spans/pageInfo/hasNextPage")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_next {
                break;
            }
            let cursor = graphql_escape(
                resp
                    .pointer("/data/getProjectByName/spans/pageInfo/endCursor")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            );
            let batch_size = remaining.min(page_size);
            let next_query = format!(
                r#"{{ getProjectByName(name: "{project_name}") {{ spans(first: {batch_size}, after: "{cursor}", rootSpansOnly: true, sort: {{col: startTime, dir: desc}}) {{ edges {{ node {{ spanId name statusCode spanKind startTime endTime latencyMs trace {{ traceId numSpans }} input {{ value truncatedValue }} output {{ value truncatedValue }} }} }} pageInfo {{ hasNextPage endCursor }} }} }} }}"#
            );
            resp = phoenix_graphql(phoenix_url, &next_query)?;
            remaining = remaining.saturating_sub(batch_size);
        }
    }

    let edges = resp
        .pointer("/data/getProjectByName/spans/edges")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let entries: Vec<TraceSummary> = edges
        .into_iter()
        .filter_map(|edge| {
            let node = edge.get("node")?;
            let trace_id = node.pointer("/trace/traceId")?.as_str()?.to_string();
            let num_spans = node.pointer("/trace/numSpans").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let status = node.get("statusCode").and_then(|v| v.as_str()).unwrap_or("UNSET").to_string();
            let kind = node
                .get("spanKind")
                .and_then(|v| v.as_str())
                .map(normalize_span_kind)
                .unwrap_or_else(|| "UNKNOWN".to_string());
            let start_time = node.get("startTime").and_then(|v| v.as_str()).map(|s| s.to_string());
            let latency_ms = node.get("latencyMs").and_then(|v| v.as_f64()).map(|f| f as u64);
            let input_summary = node
                .pointer("/input/value")
                .or_else(|| node.pointer("/input/truncatedValue"))
                .and_then(|v| v.as_str())
                .map(|s| truncate_chars(s, SUMMARY_TRUNCATE_LEN));
            let output_summary = node
                .pointer("/output/value")
                .or_else(|| node.pointer("/output/truncatedValue"))
                .and_then(|v| v.as_str())
                .map(|s| truncate_chars(s, SUMMARY_TRUNCATE_LEN));

            Some(TraceSummary {
                trace_id,
                status,
                kind,
                name,
                input_summary,
                output_summary,
                start_time,
                latency_ms,
                span_count: num_spans,
            })
        })
        .collect();

    Ok(TracePage { entries, total, page, page_size })
}

/// Query Phoenix GraphQL for a single trace with full span tree.
fn query_phoenix_trace_detail(
    phoenix_url: &str,
    trace_id: &str,
) -> Result<Option<TraceDetail>, String> {
    let escaped_trace_id = graphql_escape(trace_id);
    let query = format!(
        r#"{{ getTraceByOtelId(traceId: "{escaped_trace_id}") {{ traceId latencyMs startTime numSpans spans(first: 200) {{ edges {{ node {{ spanId name spanKind statusCode startTime endTime latencyMs parentId input {{ value truncatedValue }} output {{ value truncatedValue }} attributes }} }} }} }} }}"#
    );
    let resp = phoenix_graphql(phoenix_url, &query)?;
    let trace = match resp.pointer("/data/getTraceByOtelId") {
        Some(t) if !t.is_null() => t,
        _ => return Ok(None),
    };

    let status = trace
        .pointer("/spans/edges/0/node/statusCode")
        .and_then(|v| v.as_str())
        .unwrap_or("UNSET")
        .to_string();
    let start_time = trace.get("startTime").and_then(|v| v.as_str()).map(|s| s.to_string());
    let latency_ms = trace.get("latencyMs").and_then(|v| v.as_f64()).map(|f| f as u64);

    let edges = trace
        .pointer("/spans/edges")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let raw_spans: Vec<RawSpan> = edges
        .into_iter()
        .filter_map(|edge| {
            let node = edge.get("node")?;
            let span_id = node.get("spanId")?.as_str()?.to_string();
            let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let kind = node
                .get("spanKind")
                .and_then(|v| v.as_str())
                .map(normalize_span_kind)
                .unwrap_or_else(|| "UNKNOWN".to_string());
            let status = node.get("statusCode").and_then(|v| v.as_str()).unwrap_or("UNSET").to_string();
            let start_time = node.get("startTime").and_then(|v| v.as_str()).map(|s| s.to_string());
            let end_time = node.get("endTime").and_then(|v| v.as_str()).map(|s| s.to_string());
            let parent_span_id = node.get("parentId").and_then(|v| v.as_str()).map(|s| s.to_string());
            let input = node
                .pointer("/input/value")
                .or_else(|| node.pointer("/input/truncatedValue"))
                .and_then(|v| v.as_str())
                .map(|s| serde_json::Value::String(s.to_string()));
            let output = node
                .pointer("/output/value")
                .or_else(|| node.pointer("/output/truncatedValue"))
                .and_then(|v| v.as_str())
                .map(|s| serde_json::Value::String(s.to_string()));
            let attributes = node.get("attributes").cloned();

            Some(RawSpan {
                trace_id: trace_id.to_string(),
                span_id,
                parent_span_id,
                name,
                kind,
                status,
                start_time,
                end_time,
                input,
                output,
                attributes,
            })
        })
        .collect();

    let spans = build_span_tree(&raw_spans);

    Ok(Some(TraceDetail {
        trace_id: trace_id.to_string(),
        status,
        latency_ms,
        start_time,
        spans,
    }))
}

// ---------------------------------------------------------------------------
// Trace commands
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AtofEventPage {
    pub entries: Vec<AtofJsonLine>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
}

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AtofJsonLine {
    pub line_number: usize,
    pub raw: String,
}

/// Parse the ATOF archive into trace summaries for the list page.
/// Groups raw spans by trace_id and returns one row per trace.
#[tauri::command]
fn list_atof_traces(work_dir: String, page: usize, page_size: usize) -> Result<TracePage, String> {
    parse_trace_summaries(&work_dir, page, page_size)
}

/// Parse the ATOF archive and return a single trace with full span tree.
#[tauri::command]
fn get_atof_trace_detail(work_dir: String, trace_id: String) -> Result<Option<TraceDetail>, String> {
    parse_trace_detail(&work_dir, &trace_id)
}

/// Return raw ATOF events in the append order produced by NeMo Relay.
#[tauri::command]
fn read_atof_events(work_dir: String, limit: usize, offset: usize) -> Result<AtofEventPage, String> {
    let path = atof_path(&work_dir);
    if !path.exists() {
        return Ok(AtofEventPage { entries: Vec::new(), total: 0, offset, limit, has_more: false });
    }
    let page_size = if limit == 0 { 40 } else { limit.min(100) };
    let file = fs::File::open(&path).map_err(|e| format!("Failed to read ATOF: {e}"))?;
    let reader = BufReader::new(file);
    let mut entries = Vec::with_capacity(page_size);
    let mut total = 0usize;

    for line in reader.lines() {
        let line = line.map_err(|e| format!("Failed to read ATOF: {e}"))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let line_number = total + 1;
        total += 1;
        if total <= offset || entries.len() >= page_size {
            continue;
        }
        entries.push(AtofJsonLine { line_number, raw: line.to_string() });
    }

    let start = offset.min(total);
    let end = start + entries.len();

    Ok(AtofEventPage {
        entries,
        total,
        offset: start,
        limit: page_size,
        has_more: end < total,
    })
}

/// Query Phoenix GraphQL for trace summaries when ATOF is unavailable.
#[tauri::command]
fn query_phoenix_traces(
    phoenix_url: String,
    service_name: Option<String>,
    page: usize,
    page_size: usize,
) -> Result<TracePage, String> {
    let project = discover_phoenix_project(&phoenix_url, service_name.as_deref())?
        .ok_or_else(|| "No Phoenix project with traces found".to_string())?;
    query_phoenix_trace_summaries(&phoenix_url, &project, page, page_size)
}

/// Query Phoenix GraphQL for a single trace detail when ATOF is unavailable.
#[tauri::command]
fn query_phoenix_trace_detail_cmd(
    phoenix_url: String,
    trace_id: String,
) -> Result<Option<TraceDetail>, String> {
    query_phoenix_trace_detail(&phoenix_url, &trace_id)
}

#[cfg(test)]
mod tests_traces {
    use super::*;

    #[test]
    fn truncate_chars_handles_multibyte() {
        // Multi-byte characters must never be cut in the middle.
        let emoji = "🔥".repeat(60); // 60 chars, 240 bytes
        let truncated = truncate_chars(&emoji, 50);
        assert_eq!(truncated.chars().count(), 50);
        assert!(truncated.ends_with("..."));
        assert!(truncated.is_char_boundary(truncated.len()));
        // Short strings and exact-limit strings pass through unchanged.
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars(&emoji, 60), emoji);
    }
    #[test]
    fn graphql_escape_handles_quotes_backslashes_and_line_terminators() {
        // Quotes and backslashes are the only characters that can break out of
        // a GraphQL string literal; line terminators must also be escaped to
        // keep the query syntactically valid.
        assert_eq!(graphql_escape("plain-name"), "plain-name");
        assert_eq!(graphql_escape("a\"b"), "a\\\"b");
        assert_eq!(graphql_escape("a\\b"), "a\\\\b");
        assert_eq!(graphql_escape("a\nb"), "a\\nb");
        assert_eq!(graphql_escape("a\rb"), "a\\rb");
        assert_eq!(graphql_escape("a\tb"), "a\\tb");
        // A mixed input must not produce an unescaped quote or newline.
        let escaped = graphql_escape("x\"y\nz\\w");
        assert!(!escaped.contains('"') || escaped.contains("\\\""));
        assert!(!escaped.contains('\n'));
    }
    #[test]
    fn parse_iso_millis_formats() {
        // ISO-8601 with Z suffix and fractional seconds.
        assert_eq!(parse_iso_millis("2025-01-01T00:00:00Z"), Some(1_735_689_600_000));
        assert_eq!(parse_iso_millis("2025-01-01T00:00:00.123Z"), Some(1_735_689_600_123));
        // Numeric seconds (< 1e12) are converted to millis; millis pass through.
        assert_eq!(parse_iso_millis("1735689600"), Some(1_735_689_600_000));
        assert_eq!(parse_iso_millis("1735689600000"), Some(1_735_689_600_000));
        // Empty, malformed and out-of-range inputs return None.
        assert_eq!(parse_iso_millis(""), None);
        assert_eq!(parse_iso_millis("  "), None);
        assert_eq!(parse_iso_millis("not-a-date"), None);
        assert_eq!(parse_iso_millis("2025-13-01T00:00:00Z"), None); // month 13
        assert_eq!(parse_iso_millis("2025-01-01T25:00:00Z"), None); // hour 25
    }
    #[test]
    fn parse_atof_spans_groups_scopes_into_traces() {
        // Two traces (chat ids) each rooted at an agent event; a tool scope
        // nested under the first trace's agent via parent_uuid.
        let content = r#"
{"atof_version":"0.1","category":"agent","name":"agentseek","uuid":"a-root-1","parent_uuid":null,"scope_category":"start","timestamp":"2026-08-05T11:08:53.180467288+00:00","data":{"context":"channel=$ag-ui|chat_id=trace-one","session_id":"trace-one"}}
{"atof_version":"0.1","category":"agent","name":"agentseek","uuid":"a-root-1","parent_uuid":null,"scope_category":"end","timestamp":"2026-08-05T11:08:58.180467288+00:00","data":{"messages":[]}}
{"atof_version":"0.1","category":"tool","name":"tavily_search","uuid":"a-tool-1","parent_uuid":"a-root-1","scope_category":"start","timestamp":"2026-08-05T11:08:54.000000000+00:00","data":{"query":"hello"}}
{"atof_version":"0.1","category":"tool","name":"tavily_search","uuid":"a-tool-1","parent_uuid":"a-root-1","scope_category":"end","timestamp":"2026-08-05T11:08:55.000000000+00:00","data":{"query":"hello","error":"boom"}}
{"atof_version":"0.1","category":"llm","name":"model","uuid":"a-model-1","parent_uuid":"a-tool-1","scope_category":"start","timestamp":"2026-08-05T11:08:54.100000000+00:00","data":{"prompt":"nested"}}
{"atof_version":"0.1","category":"llm","name":"model","uuid":"a-model-1","parent_uuid":"a-tool-1","scope_category":"end","timestamp":"2026-08-05T11:08:54.900000000+00:00","data":{"response":"ok"}}
{"atof_version":"0.1","category":"agent","name":"agentseek","uuid":"b-root-1","parent_uuid":null,"scope_category":"start","timestamp":"2026-08-05T11:09:00.000000000+00:00","data":{"session_id":"trace-one"}}
{"atof_version":"0.1","category":"agent","name":"agentseek","uuid":"b-root-1","parent_uuid":null,"scope_category":"end","timestamp":"2026-08-05T11:09:02.000000000+00:00","data":{}}
"#;
        let spans = parse_atof_spans(content);
        assert_eq!(spans.len(), 4);

        // Root agent of trace one: its root UUID is the trace id, with OK status.
        let root = spans.iter().find(|s| s.span_id == "a-root-1").expect("root span");
        assert_eq!(root.trace_id, "a-root-1");
        assert_eq!(root.kind, "AGENT");
        assert_eq!(root.status, "OK");
        assert!(root.parent_span_id.is_none());
        assert!(root.input.is_some());
        assert!(root.output.is_some());
        assert_eq!(
            duration_ms(root.start_time.as_deref(), root.end_time.as_deref()),
            Some(5_000)
        );

        // Tool span inherits trace one, links to the root, and reports ERROR
        // because its end event carries an error field.
        let tool = spans.iter().find(|s| s.span_id == "a-tool-1").expect("tool span");
        assert_eq!(tool.trace_id, "a-root-1");
        assert_eq!(tool.parent_span_id.as_deref(), Some("a-root-1"));
        assert_eq!(tool.kind, "TOOL");
        assert_eq!(tool.status, "ERROR");

        let tree = build_span_tree(&spans);
        let root_node = tree.iter().find(|node| node.span_id == "a-root-1").expect("root node");
        assert_eq!(root_node.children.len(), 1);
        assert_eq!(root_node.children[0].span_id, "a-tool-1");
        assert_eq!(root_node.children[0].children.len(), 1);
        assert_eq!(root_node.children[0].children[0].span_id, "a-model-1");

        // Second trace root gets its own trace id.
        let other = spans.iter().find(|s| s.span_id == "b-root-1").expect("other root");
        assert_eq!(other.trace_id, "b-root-1");
    }
    #[test]
    fn parse_trace_summaries_falls_back_to_atof_events() {
        let root = std::env::temp_dir().join(format!(
            "agentseek-desktop-atof-fallback-{}",
            unique_stamp()
        ));
        let atof_dir = root.join(".nemo-relay/atof");
        fs::create_dir_all(&atof_dir).expect("create atof dir");
        fs::write(
            atof_dir.join("events.jsonl"),
            "{\"atof_version\":\"0.1\",\"category\":\"agent\",\"name\":\"agentseek\",\"uuid\":\"r-1\",\"parent_uuid\":null,\"scope_category\":\"start\",\"timestamp\":\"2026-08-05T11:08:53+00:00\",\"data\":{\"context\":\"channel=$ag-ui|chat_id=t-1\",\"session_id\":\"t-1\"}}\n\
{\"atof_version\":\"0.1\",\"category\":\"agent\",\"name\":\"agentseek\",\"uuid\":\"r-1\",\"parent_uuid\":null,\"scope_category\":\"end\",\"timestamp\":\"2026-08-05T11:08:54+00:00\",\"data\":{}}\n",
        )
        .expect("write events");
        let page = parse_trace_summaries(&root.to_string_lossy(), 1, 20).expect("summaries");
        assert_eq!(page.total, 1);
        assert_eq!(page.entries.len(), 1);
        let entry = &page.entries[0];
        assert_eq!(entry.trace_id, "r-1");
        assert_eq!(entry.status, "OK");
        assert_eq!(entry.kind, "AGENT");
        assert_eq!(entry.latency_ms, Some(1_000));
        assert_eq!(entry.span_count, 1);
        fs::remove_dir_all(root).expect("remove test directory");
    }
}
