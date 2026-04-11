use crate::error::{AppError, ErrorCode, io_error};
use crate::paths::ResolvedPaths;
use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

const TITLE_CHAR_LIMIT: usize = 72;
const SNIPPET_CHAR_LIMIT: usize = 140;
const SEARCH_TEXT_BYTE_LIMIT: usize = 64_000;
const SEARCH_TEXT_MESSAGE_LIMIT: usize = 48;
const COMPACT_SUMMARY_TEXT_LIMIT: usize = 16_000;

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredFile {
    pub thread_id: String,
    pub path: Utf8PathBuf,
    pub project_slug: String,
    pub is_subagent: bool,
    pub parent_thread_id: Option<String>,
    pub agent_slug: Option<String>,
    pub size: i64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub display: String,
    pub project_cwd: Option<String>,
    pub timestamp_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ArchiveInventory {
    pub files: Vec<DiscoveredFile>,
    pub history: BTreeMap<String, HistoryEntry>,
    pub history_mtime_ns: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct IndexedThread {
    pub thread_id: String,
    pub project_slug: String,
    pub project_cwd: Option<String>,
    pub path: Utf8PathBuf,
    pub is_subagent: bool,
    pub parent_thread_id: Option<String>,
    pub agent_slug: Option<String>,
    pub default_scope: bool,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub message_count: i64,
    pub event_count: i64,
    pub cli_version: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub was_compacted: bool,
    pub search_text: String,
}

#[derive(Debug, Clone)]
pub struct IndexedMessage {
    pub message_id: String,
    pub thread_id: String,
    pub ordinal: i64,
    pub prompt_id: Option<String>,
    pub role: String,
    pub kind: String,
    pub timestamp: Option<String>,
    pub text: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct IndexedEvent {
    pub event_id: String,
    pub thread_id: String,
    pub ordinal: i64,
    pub timestamp: Option<String>,
    pub record_type: String,
    pub file_path: Utf8PathBuf,
    pub byte_start: i64,
    pub byte_len: i64,
}

#[derive(Debug, Clone)]
pub struct ParsedThread {
    pub thread: IndexedThread,
    pub messages: Vec<IndexedMessage>,
    pub events: Vec<IndexedEvent>,
}

pub fn discover_archives(paths: &ResolvedPaths) -> Result<ArchiveInventory, AppError> {
    if !paths.projects_root.exists() {
        return Err(AppError::with_details(
            ErrorCode::ArchiveNotFound,
            "could not find Claude projects root",
            serde_json::json!({
                "projects_root": paths.projects_root,
            }),
        ));
    }

    let mut files = Vec::new();
    collect_files(&paths.projects_root, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let (history, history_mtime_ns) = load_history(paths)?;

    Ok(ArchiveInventory {
        files,
        history,
        history_mtime_ns,
    })
}

pub fn parse_thread(
    file: &DiscoveredFile,
    history: Option<&HistoryEntry>,
) -> Result<ParsedThread, AppError> {
    let handle = File::open(&file.path)
        .map_err(|error| io_error(&format!("failed to open {}", file.path), error))?;
    let mut reader = BufReader::new(handle);

    let mut line = String::new();
    let mut offset: i64 = 0;
    let mut event_ordinal: i64 = 0;
    let mut message_ordinal: i64 = 0;
    let mut messages = Vec::new();
    let mut events = Vec::new();

    let mut started_at: Option<String> = None;
    let mut updated_at: Option<String> = None;
    let mut cli_version: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut entrypoint: Option<String> = None;
    let mut project_cwd: Option<String> = None;
    let mut derived_title: Option<String> = None;
    let mut was_compacted = false;

    loop {
        line.clear();
        let bytes_read = reader
            .read_line(&mut line)
            .map_err(|error| io_error(&format!("failed to read {}", file.path), error))?;
        if bytes_read == 0 {
            break;
        }

        let byte_start = offset;
        offset += bytes_read as i64;
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "warning: skipping malformed JSONL in {} at byte {byte_start}: {error}",
                    file.path
                );
                continue;
            }
        };

        event_ordinal += 1;

        let record_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        if started_at.is_none() {
            started_at = timestamp.clone();
        }
        if let Some(ts) = timestamp.as_deref()
            && updated_at.as_deref().map(|prev| prev < ts).unwrap_or(true)
        {
            updated_at = Some(ts.to_string());
        }
        if cli_version.is_none() {
            cli_version = value
                .get("version")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        if git_branch.is_none() {
            git_branch = value
                .get("gitBranch")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        if entrypoint.is_none() {
            entrypoint = value
                .get("entrypoint")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        if project_cwd.is_none() {
            project_cwd = value
                .get("cwd")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }

        let prompt_id = value
            .get("promptId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        if record_type == "user"
            && value
                .get("isCompactSummary")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            was_compacted = true;
            if let Some(text) = extract_compact_summary_text(&value) {
                message_ordinal += 1;
                let trimmed_text = if text.len() > COMPACT_SUMMARY_TEXT_LIMIT {
                    let mut cut = COMPACT_SUMMARY_TEXT_LIMIT;
                    while !text.is_char_boundary(cut) && cut > 0 {
                        cut -= 1;
                    }
                    format!("{}...", &text[..cut])
                } else {
                    text.clone()
                };
                messages.push(IndexedMessage {
                    message_id: format!("{}:m:{message_ordinal}", file.thread_id),
                    thread_id: file.thread_id.clone(),
                    ordinal: message_ordinal,
                    prompt_id: prompt_id.clone(),
                    role: "user".to_string(),
                    kind: "compact_summary".to_string(),
                    timestamp: timestamp.clone(),
                    snippet: snippet_from_text(&text, SNIPPET_CHAR_LIMIT),
                    text: trimmed_text,
                });
            }
        } else if record_type == "user"
            && !value.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
        {
            if let Some(text) = extract_user_message_text(&value) {
                let normalized = text.trim().to_string();
                if !normalized.is_empty() && !is_local_command_noise(&normalized) {
                    if derived_title.is_none() {
                        derived_title = Some(snippet_from_text(&normalized, TITLE_CHAR_LIMIT));
                    }
                    message_ordinal += 1;
                    messages.push(IndexedMessage {
                        message_id: format!("{}:m:{message_ordinal}", file.thread_id),
                        thread_id: file.thread_id.clone(),
                        ordinal: message_ordinal,
                        prompt_id: prompt_id.clone(),
                        role: "user".to_string(),
                        kind: "user_message".to_string(),
                        timestamp: timestamp.clone(),
                        snippet: snippet_from_text(&normalized, SNIPPET_CHAR_LIMIT),
                        text: normalized,
                    });
                }
            }
        } else if record_type == "assistant"
            && let Some(text) = extract_assistant_text(&value)
        {
            let normalized = text.trim().to_string();
            if !normalized.is_empty() {
                message_ordinal += 1;
                messages.push(IndexedMessage {
                    message_id: format!("{}:m:{message_ordinal}", file.thread_id),
                    thread_id: file.thread_id.clone(),
                    ordinal: message_ordinal,
                    prompt_id: prompt_id.clone(),
                    role: "assistant".to_string(),
                    kind: "assistant_text".to_string(),
                    timestamp: timestamp.clone(),
                    snippet: snippet_from_text(&normalized, SNIPPET_CHAR_LIMIT),
                    text: normalized,
                });
            }
        }

        events.push(IndexedEvent {
            event_id: format!("{}:e:{event_ordinal}", file.thread_id),
            thread_id: file.thread_id.clone(),
            ordinal: event_ordinal,
            timestamp,
            record_type,
            file_path: file.path.clone(),
            byte_start,
            byte_len: bytes_read as i64,
        });
    }

    let title = derived_title
        .or_else(|| history.map(|entry| snippet_from_text(&entry.display, TITLE_CHAR_LIMIT)))
        .or_else(|| Some(file.thread_id.clone()));

    let project_cwd = project_cwd.or_else(|| history.and_then(|entry| entry.project_cwd.clone()));

    let search_text = build_thread_search_text(title.as_deref(), &messages);

    Ok(ParsedThread {
        thread: IndexedThread {
            thread_id: file.thread_id.clone(),
            project_slug: file.project_slug.clone(),
            project_cwd,
            path: file.path.clone(),
            is_subagent: file.is_subagent,
            parent_thread_id: file.parent_thread_id.clone(),
            agent_slug: file.agent_slug.clone(),
            default_scope: !file.is_subagent,
            title,
            started_at,
            updated_at,
            message_count: messages.len() as i64,
            event_count: events.len() as i64,
            cli_version,
            git_branch,
            entrypoint,
            was_compacted,
            search_text,
        },
        messages,
        events,
    })
}

fn collect_files(
    projects_root: &Utf8PathBuf,
    files: &mut Vec<DiscoveredFile>,
) -> Result<(), AppError> {
    let walker = WalkDir::new(projects_root).follow_links(false);
    for entry in walker.into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let raw_path = entry.path();
        if raw_path.extension().and_then(|item| item.to_str()) != Some("jsonl") {
            continue;
        }
        let path = match Utf8PathBuf::from_path_buf(raw_path.to_path_buf()) {
            Ok(value) => value,
            Err(_) => {
                eprintln!(
                    "warning: skipping non-UTF-8 archive path: {}",
                    raw_path.display()
                );
                continue;
            }
        };

        let Some(classified) = classify_file(projects_root, &path) else {
            continue;
        };

        let metadata = entry
            .metadata()
            .map_err(|error| io_error(&format!("failed to stat {path}"), error.into()))?;
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos() as i64)
            .unwrap_or_default();
        let size = metadata.len() as i64;

        files.push(DiscoveredFile {
            thread_id: classified.thread_id,
            path,
            project_slug: classified.project_slug,
            is_subagent: classified.is_subagent,
            parent_thread_id: classified.parent_thread_id,
            agent_slug: classified.agent_slug,
            size,
            mtime_ns,
        });
    }
    Ok(())
}

struct ClassifiedFile {
    project_slug: String,
    thread_id: String,
    is_subagent: bool,
    parent_thread_id: Option<String>,
    agent_slug: Option<String>,
}

fn classify_file(projects_root: &Utf8PathBuf, path: &Utf8PathBuf) -> Option<ClassifiedFile> {
    let rel = path.strip_prefix(projects_root).ok()?;
    let mut components: Vec<&str> = rel.components().map(|component| component.as_str()).collect();
    let file_name = components.pop()?.to_string();
    let stem = file_name.strip_suffix(".jsonl")?.to_string();

    match components.as_slice() {
        [project_slug] => Some(ClassifiedFile {
            project_slug: (*project_slug).to_string(),
            thread_id: stem,
            is_subagent: false,
            parent_thread_id: None,
            agent_slug: None,
        }),
        [project_slug, parent_session_id, "subagents"] => {
            let (slug, hash) = parse_subagent_filename(&stem)?;
            Some(ClassifiedFile {
                project_slug: (*project_slug).to_string(),
                thread_id: format!("{parent_session_id}:agent:{hash}"),
                is_subagent: true,
                parent_thread_id: Some((*parent_session_id).to_string()),
                agent_slug: slug,
            })
        }
        _ => None,
    }
}

fn parse_subagent_filename(stem: &str) -> Option<(Option<String>, String)> {
    let rest = stem.strip_prefix("agent-")?;
    if let Some((slug, hash)) = rest.rsplit_once('-') {
        Some((Some(slug.to_string()), hash.to_string()))
    } else {
        Some((None, rest.to_string()))
    }
}

fn load_history(
    paths: &ResolvedPaths,
) -> Result<(BTreeMap<String, HistoryEntry>, Option<i64>), AppError> {
    if !paths.history_path.exists() {
        return Ok((BTreeMap::new(), None));
    }

    let metadata = std::fs::metadata(&paths.history_path)
        .map_err(|error| io_error("failed to stat history.jsonl", error))?;
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos() as i64);

    let file = File::open(&paths.history_path)
        .map_err(|error| io_error("failed to open history.jsonl", error))?;
    let reader = BufReader::new(file);
    let mut entries: BTreeMap<String, HistoryEntry> = BTreeMap::new();
    for (line_number, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| io_error("failed to read history.jsonl", error))?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "warning: ignoring malformed history.jsonl line {}: {}",
                    line_number + 1,
                    error
                );
                continue;
            }
        };
        let Some(session_id) = value.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        let display = match value.get("display").and_then(Value::as_str) {
            Some(text) if !text.trim().is_empty() => text.trim().to_string(),
            _ => continue,
        };
        if is_local_command_noise(&display) {
            continue;
        }
        let project_cwd = value
            .get("project")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let timestamp_ms = value.get("timestamp").and_then(Value::as_i64);

        let entry = HistoryEntry {
            display,
            project_cwd,
            timestamp_ms,
        };
        match entries.get(session_id) {
            Some(existing)
                if existing
                    .timestamp_ms
                    .zip(entry.timestamp_ms)
                    .map(|(a, b)| a >= b)
                    .unwrap_or(false) => {}
            _ => {
                entries.insert(session_id.to_string(), entry);
            }
        }
    }
    Ok((entries, mtime_ns))
}

fn extract_user_message_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let array = content.as_array()?;
    let mut parts = Vec::new();
    for item in array {
        let block_type = item.get("type").and_then(Value::as_str);
        match block_type {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    parts.push(text.trim().to_string());
                }
            }
            Some("tool_result") => {
                // tool_result wrappers are not user intent; ignore for messages
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn extract_assistant_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    let array = content.as_array()?;
    let mut parts = Vec::new();
    for item in array {
        if item.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = item.get("text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            parts.push(text.trim().to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn extract_compact_summary_text(value: &Value) -> Option<String> {
    extract_user_message_text(value)
}

fn is_local_command_noise(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<command-message>")
        || trimmed.starts_with("<command-args>")
}

fn build_thread_search_text(title: Option<&str>, messages: &[IndexedMessage]) -> String {
    let mut search = String::new();
    if let Some(title) = title {
        search.push_str(title);
        search.push('\n');
    }
    for message in messages.iter().take(SEARCH_TEXT_MESSAGE_LIMIT) {
        search.push_str(&message.text);
        search.push('\n');
        if search.len() > SEARCH_TEXT_BYTE_LIMIT {
            break;
        }
    }
    search
}

pub fn snippet_from_text(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = normalized.chars().count();
    if char_count <= limit {
        return normalized;
    }
    let take_count = limit.saturating_sub(3);
    let clipped = normalized.chars().take(take_count).collect::<String>();
    format!("{clipped}...")
}
