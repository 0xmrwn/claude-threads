use crate::error::{AppError, ErrorCode, io_error};
use crate::paths::ResolvedPaths;
use camino::{Utf8Path, Utf8PathBuf};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

const TITLE_CHAR_LIMIT: usize = 72;
const EXPLICIT_TITLE_CHAR_LIMIT: usize = 200;
const SNIPPET_CHAR_LIMIT: usize = 140;
const SEARCH_TEXT_BYTE_LIMIT: usize = 64_000;
const SEARCH_TEXT_MESSAGE_LIMIT: usize = 48;
const COMPACT_SUMMARY_TEXT_LIMIT: usize = 16_000;

pub const SOURCE_CLAUDE_CODE: &str = "claude_code";
pub const SOURCE_COWORK: &str = "cowork";
pub const COWORK_PROJECT_SLUG: &str = "cowork";
const COWORK_PROJECT_NAME: &str = "Cowork";
const SYNTHETIC_MODEL: &str = "<synthetic>";

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredFile {
    pub thread_id: String,
    pub path: Utf8PathBuf,
    pub source: &'static str,
    /// Directory slug under `projects/`. For Cowork transcripts this is the
    /// VM-internal slug; the indexed project comes from session metadata.
    pub project_slug: String,
    pub is_subagent: bool,
    pub parent_thread_id: Option<String>,
    pub agent_slug: Option<String>,
    pub workflow_id: Option<String>,
    /// Cowork session directory (`{org}/{session}`) that owns this transcript.
    pub cowork_session_dir: Option<Utf8PathBuf>,
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
    /// Enrichment files (history, sidecars, desktop session metadata) keyed by
    /// path with their `(size, mtime_ns)`; used for freshness and lookups.
    pub metadata_files: BTreeMap<Utf8PathBuf, (i64, i64)>,
    pub metadata_fingerprint: String,
}

/// Session metadata written by the Claude desktop app for Code tab sessions
/// (`claude-code-sessions/**/local_*.json`) and Cowork sessions
/// (`local-agent-mode-sessions/**/local_*.json`).
#[derive(Debug, Clone)]
struct AppSession {
    app_session_id: String,
    cli_session_id: Option<String>,
    title: Option<String>,
    is_archived: bool,
    last_activity_at: Option<i64>,
    space_id: Option<String>,
    selected_folders: Vec<String>,
    cwd: Option<Utf8PathBuf>,
}

#[derive(Debug, Clone)]
struct CoworkSpace {
    name: Option<String>,
    folders: Vec<String>,
}

/// Enrichment sources loaded once per sync.
#[derive(Debug, Default)]
pub struct ArchiveMetadata {
    history: BTreeMap<String, HistoryEntry>,
    desktop_sessions: BTreeMap<String, AppSession>,
    cowork_sessions: BTreeMap<Utf8PathBuf, AppSession>,
    cowork_spaces: BTreeMap<String, CoworkSpace>,
}

/// Metadata resolved for one transcript. Serialized into the index so a
/// metadata-only change (rename, archive, history) re-parses just that thread.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ThreadMetadata {
    pub project_slug: Option<String>,
    pub project_name: Option<String>,
    pub project_cwd: Option<String>,
    pub selected_folders: Vec<String>,
    pub app_session_id: Option<String>,
    pub app_title: Option<String>,
    pub is_archived: bool,
    pub custom_title: Option<String>,
    pub agent_type: Option<String>,
    pub agent_description: Option<String>,
    pub history_display: Option<String>,
    pub history_project: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IndexedThread {
    pub thread_id: String,
    pub source: String,
    pub project_slug: String,
    pub project_name: Option<String>,
    pub project_cwd: Option<String>,
    pub path: Utf8PathBuf,
    pub is_subagent: bool,
    pub parent_thread_id: Option<String>,
    pub agent_slug: Option<String>,
    pub agent_type: Option<String>,
    pub workflow_id: Option<String>,
    pub default_scope: bool,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub message_count: i64,
    pub event_count: i64,
    pub cli_version: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub model: Option<String>,
    pub app_session_id: Option<String>,
    pub is_archived: bool,
    pub selected_folders: Vec<String>,
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
    /// True when the transcript holds no user/assistant records at all (for
    /// example `ai-title`-only stubs left by non-persisted `claude -p` runs).
    pub is_empty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageClass {
    UserMessage,
    TaskNotification,
    PeerMessage,
    SystemEvent,
}

impl MessageClass {
    fn role(self) -> &'static str {
        match self {
            Self::UserMessage => "user",
            Self::TaskNotification | Self::PeerMessage | Self::SystemEvent => "system",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::TaskNotification => "task_notification",
            Self::PeerMessage => "peer_message",
            Self::SystemEvent => "system_event",
        }
    }
}

pub fn discover_archives(paths: &ResolvedPaths) -> Result<ArchiveInventory, AppError> {
    let cowork_root = paths.cowork_root.as_ref().filter(|root| root.is_dir());
    if !paths.projects_root.exists() && cowork_root.is_none() {
        return Err(AppError::with_details(
            ErrorCode::ArchiveNotFound,
            "could not find Claude projects root",
            serde_json::json!({
                "projects_root": paths.projects_root,
                "cowork_root": paths.cowork_root,
            }),
        ));
    }

    let mut metadata_files = BTreeMap::new();
    let mut files = Vec::new();
    if paths.projects_root.exists() {
        collect_project_files(
            &paths.projects_root,
            SOURCE_CLAUDE_CODE,
            None,
            &mut files,
            &mut metadata_files,
        )?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    if let Some(root) = cowork_root {
        let mut cowork_files = Vec::new();
        collect_cowork_files(root, &mut cowork_files, &mut metadata_files)?;
        cowork_files.sort_by(|a, b| a.path.cmp(&b.path));
        files.extend(cowork_files);
    }
    if let Some(root) = paths.desktop_sessions_root.as_ref() {
        for org_dir in child_dirs(root).iter().flat_map(|acct| child_dirs(acct)) {
            for (path, is_dir) in dir_entries(&org_dir) {
                if !is_dir && is_app_session_file(&path) {
                    record_metadata_file(&path, &mut metadata_files);
                }
            }
        }
    }
    record_metadata_file(&paths.history_path, &mut metadata_files);

    let files = dedupe_thread_ids(files);
    let metadata_fingerprint = fingerprint(&metadata_files);
    Ok(ArchiveInventory {
        files,
        metadata_files,
        metadata_fingerprint,
    })
}

pub fn load_metadata(
    paths: &ResolvedPaths,
    inventory: &ArchiveInventory,
) -> Result<ArchiveMetadata, AppError> {
    let mut metadata = ArchiveMetadata {
        history: load_history(paths)?,
        ..ArchiveMetadata::default()
    };

    for path in inventory.metadata_files.keys() {
        let under_desktop = paths
            .desktop_sessions_root
            .as_ref()
            .is_some_and(|root| path.starts_with(root));
        let under_cowork = paths
            .cowork_root
            .as_ref()
            .is_some_and(|root| path.starts_with(root));
        if !under_desktop && !under_cowork {
            continue;
        }
        if path.file_name() == Some("spaces.json") {
            load_cowork_spaces(path, &mut metadata.cowork_spaces);
            continue;
        }
        if !is_app_session_file(path) {
            continue;
        }
        let Some(session) = read_app_session(path) else {
            continue;
        };
        if under_desktop {
            let Some(cli_session_id) = session.cli_session_id.clone() else {
                continue;
            };
            match metadata.desktop_sessions.get(&cli_session_id) {
                Some(existing) if existing.last_activity_at >= session.last_activity_at => {}
                _ => {
                    metadata.desktop_sessions.insert(cli_session_id, session);
                }
            }
        } else if let Some(org_dir) = path.parent() {
            for dir in cowork_session_dirs(org_dir, path, &session) {
                metadata.cowork_sessions.insert(dir, session.clone());
            }
        }
    }
    Ok(metadata)
}

pub fn resolve_metadata(
    metadata: &ArchiveMetadata,
    inventory: &ArchiveInventory,
    file: &DiscoveredFile,
) -> ThreadMetadata {
    let mut resolved = ThreadMetadata::default();

    if file.is_subagent {
        let sidecar = Utf8PathBuf::from(format!(
            "{}.meta.json",
            file.path.as_str().trim_end_matches(".jsonl")
        ));
        if inventory.metadata_files.contains_key(&sidecar)
            && let Some(raw) = read_json_file::<RawAgentMeta>(&sidecar)
        {
            resolved.agent_type = non_empty(raw.agent_type);
            resolved.agent_description = non_empty(raw.description);
        }
    } else {
        let sidecar = file.path.with_extension("").join("custom-title.json");
        if inventory.metadata_files.contains_key(&sidecar)
            && let Some(raw) = read_json_file::<RawCustomTitle>(&sidecar)
        {
            resolved.custom_title = non_empty(raw.custom_title);
        }
    }

    if file.source == SOURCE_COWORK {
        let session = file
            .cowork_session_dir
            .as_ref()
            .and_then(|dir| metadata.cowork_sessions.get(dir));
        let space_id = session.and_then(|session| session.space_id.clone());
        let space = space_id
            .as_ref()
            .and_then(|id| metadata.cowork_spaces.get(id));
        let first_selected = session.and_then(|session| session.selected_folders.first().cloned());
        match space_id {
            Some(space_id) => {
                resolved.project_slug = Some(format!("{COWORK_PROJECT_SLUG}:{space_id}"));
                resolved.project_name = space.and_then(|space| space.name.clone());
                resolved.project_cwd = space
                    .and_then(|space| space.folders.first().cloned())
                    .or(first_selected);
            }
            None => {
                resolved.project_slug = Some(COWORK_PROJECT_SLUG.to_string());
                resolved.project_name = Some(COWORK_PROJECT_NAME.to_string());
                resolved.project_cwd = first_selected;
            }
        }
        if let Some(session) = session {
            resolved.selected_folders = session.selected_folders.clone();
            if !file.is_subagent {
                apply_app_session(&mut resolved, session);
            }
        }
    } else if !file.is_subagent {
        if let Some(session) = metadata.desktop_sessions.get(&file.thread_id) {
            apply_app_session(&mut resolved, session);
        }
        if let Some(entry) = metadata.history.get(&file.thread_id) {
            resolved.history_display = Some(entry.display.clone());
            resolved.history_project = entry.project_cwd.clone();
        }
    }
    resolved
}

pub fn parse_thread(
    file: &DiscoveredFile,
    meta: &ThreadMetadata,
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
    let mut transcript_cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut derived_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut was_compacted = false;
    let mut has_conversation = false;

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
            cli_version = string_field(&value, "version");
        }
        if git_branch.is_none() {
            git_branch = string_field(&value, "gitBranch");
        }
        if entrypoint.is_none() {
            entrypoint = string_field(&value, "entrypoint");
        }
        if transcript_cwd.is_none() {
            transcript_cwd = string_field(&value, "cwd");
        }

        let prompt_id = string_field(&value, "promptId");
        let mut sink = MessageSink {
            thread_id: &file.thread_id,
            messages: &mut messages,
            ordinal: &mut message_ordinal,
            prompt_id: &prompt_id,
            timestamp: &timestamp,
        };

        match record_type.as_str() {
            "user" => {
                has_conversation = true;
                if bool_field(&value, "isCompactSummary") {
                    was_compacted = true;
                    if let Some(text) = extract_user_message_text(&value) {
                        let stored = if text.len() > COMPACT_SUMMARY_TEXT_LIMIT {
                            let mut cut = COMPACT_SUMMARY_TEXT_LIMIT;
                            while !text.is_char_boundary(cut) && cut > 0 {
                                cut -= 1;
                            }
                            format!("{}...", &text[..cut])
                        } else {
                            text.clone()
                        };
                        sink.push("user", "compact_summary", &text, stored);
                    }
                } else if let Some(text) = extract_user_message_text(&value) {
                    let hidden = bool_field(&value, "isMeta") || bool_field(&value, "isSynthetic");
                    if let Some(class) = classify_message(&text, value.get("origin"), None, hidden)
                    {
                        let normalized = text.trim().to_string();
                        if class == MessageClass::UserMessage && derived_title.is_none() {
                            derived_title = Some(snippet_from_text(&normalized, TITLE_CHAR_LIMIT));
                        }
                        sink.push(class.role(), class.kind(), &normalized, normalized.clone());
                    }
                }
            }
            "assistant" => {
                has_conversation = true;
                let message_model = value
                    .get("message")
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str);
                let synthetic = bool_field(&value, "isApiErrorMessage")
                    || message_model == Some(SYNTHETIC_MODEL);
                if !synthetic {
                    if let Some(name) = message_model.filter(|name| !name.is_empty()) {
                        model = Some(name.to_string());
                    }
                    if let Some(text) = extract_assistant_text(&value) {
                        let normalized = text.trim().to_string();
                        if !normalized.is_empty() {
                            sink.push(
                                "assistant",
                                "assistant_text",
                                &normalized,
                                normalized.clone(),
                            );
                        }
                    }
                }
            }
            "attachment" => {
                // Prompts typed while the agent is busy are delivered as
                // `queued_command` attachments rather than `user` records.
                if let Some(attachment) = value.get("attachment")
                    && attachment.get("type").and_then(Value::as_str) == Some("queued_command")
                    && let Some(text) = extract_content_text(attachment.get("prompt"))
                {
                    let class = classify_message(
                        &text,
                        attachment.get("origin"),
                        attachment.get("commandMode").and_then(Value::as_str),
                        bool_field(attachment, "isMeta"),
                    );
                    if let Some(class) = class {
                        let normalized = text.trim().to_string();
                        if class == MessageClass::UserMessage && derived_title.is_none() {
                            derived_title = Some(snippet_from_text(&normalized, TITLE_CHAR_LIMIT));
                        }
                        sink.push(class.role(), class.kind(), &normalized, normalized.clone());
                    }
                }
            }
            "system" => {
                if value.get("subtype").and_then(Value::as_str) == Some("away_summary")
                    && let Some(text) = value.get("content").and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    let normalized = text.trim().to_string();
                    sink.push("system", "away_summary", &normalized, normalized.clone());
                }
            }
            "ai-title" => {
                if let Some(title) = non_empty(string_field(&value, "aiTitle")) {
                    ai_title = Some(title);
                }
            }
            "custom-title" => {
                if let Some(title) = non_empty(string_field(&value, "customTitle")) {
                    custom_title = Some(title);
                }
            }
            _ => {}
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

    let explicit_title = |title: &String| snippet_from_text(title, EXPLICIT_TITLE_CHAR_LIMIT);
    let history_title = meta
        .history_display
        .as_ref()
        .map(|display| snippet_from_text(display, TITLE_CHAR_LIMIT));
    let title_candidates = [
        custom_title.as_ref().map(explicit_title),
        meta.custom_title.as_ref().map(explicit_title),
        meta.app_title.as_ref().map(explicit_title),
        ai_title.as_ref().map(explicit_title),
        meta.agent_description.as_ref().map(explicit_title),
        derived_title,
        history_title,
    ];
    let title = title_candidates
        .iter()
        .flatten()
        .find(|candidate| !candidate.is_empty())
        .cloned()
        .or_else(|| Some(file.thread_id.clone()));

    let (project_slug, project_cwd) = if file.source == SOURCE_COWORK {
        (
            meta.project_slug
                .clone()
                .unwrap_or_else(|| COWORK_PROJECT_SLUG.to_string()),
            meta.project_cwd.clone(),
        )
    } else {
        (
            file.project_slug.clone(),
            transcript_cwd.or_else(|| meta.history_project.clone()),
        )
    };

    let search_text = build_thread_search_text(title.as_deref(), &title_candidates, &messages);

    Ok(ParsedThread {
        thread: IndexedThread {
            thread_id: file.thread_id.clone(),
            source: file.source.to_string(),
            project_slug,
            project_name: meta.project_name.clone(),
            project_cwd,
            path: file.path.clone(),
            is_subagent: file.is_subagent,
            parent_thread_id: file.parent_thread_id.clone(),
            agent_slug: file.agent_slug.clone(),
            agent_type: meta.agent_type.clone(),
            workflow_id: file.workflow_id.clone(),
            default_scope: !file.is_subagent,
            title,
            started_at,
            updated_at,
            message_count: messages.len() as i64,
            event_count: events.len() as i64,
            cli_version,
            git_branch,
            entrypoint,
            model,
            app_session_id: meta.app_session_id.clone(),
            is_archived: meta.is_archived,
            selected_folders: meta.selected_folders.clone(),
            was_compacted,
            search_text,
        },
        messages,
        events,
        is_empty: !has_conversation,
    })
}

struct MessageSink<'a> {
    thread_id: &'a str,
    messages: &'a mut Vec<IndexedMessage>,
    ordinal: &'a mut i64,
    prompt_id: &'a Option<String>,
    timestamp: &'a Option<String>,
}

impl MessageSink<'_> {
    fn push(&mut self, role: &str, kind: &str, full_text: &str, stored_text: String) {
        *self.ordinal += 1;
        let ordinal = *self.ordinal;
        self.messages.push(IndexedMessage {
            message_id: format!("{}:m:{ordinal}", self.thread_id),
            thread_id: self.thread_id.to_string(),
            ordinal,
            prompt_id: self.prompt_id.clone(),
            role: role.to_string(),
            kind: kind.to_string(),
            timestamp: self.timestamp.clone(),
            snippet: snippet_from_text(full_text, SNIPPET_CHAR_LIMIT),
            text: stored_text,
        });
    }
}

/// Classifies user-role text by who actually authored it. Returns `None` for
/// records that should not become messages (slash-command noise, hidden meta).
fn classify_message(
    text: &str,
    origin: Option<&Value>,
    command_mode: Option<&str>,
    hidden: bool,
) -> Option<MessageClass> {
    let trimmed = text.trim_start();
    if trimmed.is_empty() || is_local_command_noise(trimmed) {
        return None;
    }
    let origin_kind = origin
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str);
    let class = if origin_kind == Some("task-notification")
        || command_mode == Some("task-notification")
        || trimmed.starts_with("<task-notification")
    {
        MessageClass::TaskNotification
    } else if matches!(origin_kind, Some("peer") | Some("coordinator"))
        || trimmed.starts_with("<cross-session-message")
    {
        MessageClass::PeerMessage
    } else if [
        "<ci-monitor-event",
        "<bash-stdout",
        "<bash-stderr",
        "<system-reminder",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
    {
        MessageClass::SystemEvent
    } else {
        MessageClass::UserMessage
    };
    // Hidden (isMeta / isSynthetic) records are harness plumbing, except for
    // notifications and cross-session messages that carry real content.
    if hidden
        && !matches!(
            class,
            MessageClass::TaskNotification | MessageClass::PeerMessage
        )
    {
        return None;
    }
    Some(class)
}

fn apply_app_session(resolved: &mut ThreadMetadata, session: &AppSession) {
    resolved.app_session_id = Some(session.app_session_id.clone());
    resolved.app_title = session.title.clone();
    resolved.is_archived = session.is_archived;
}

fn collect_project_files(
    projects_root: &Utf8Path,
    source: &'static str,
    cowork_session_dir: Option<&Utf8Path>,
    files: &mut Vec<DiscoveredFile>,
    metadata_files: &mut BTreeMap<Utf8PathBuf, (i64, i64)>,
) -> Result<(), AppError> {
    let walker = WalkDir::new(projects_root).follow_links(false);
    for entry in walker.into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let raw_path = entry.path();
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
        let Some(file_name) = path.file_name() else {
            continue;
        };
        if file_name == "custom-title.json"
            || (file_name.starts_with("agent-") && file_name.ends_with(".meta.json"))
        {
            record_metadata_file(&path, metadata_files);
            continue;
        }
        if path.extension() != Some("jsonl") {
            continue;
        }

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
            source,
            project_slug: classified.project_slug,
            is_subagent: classified.is_subagent,
            parent_thread_id: classified.parent_thread_id,
            agent_slug: classified.agent_slug,
            workflow_id: classified.workflow_id,
            cowork_session_dir: cowork_session_dir.map(Utf8Path::to_path_buf),
            size,
            mtime_ns,
        });
    }
    Ok(())
}

/// Cowork layout: `{root}/{account}/{org}/{session}/.claude/projects/...`, with
/// session metadata in `{org}/local_{uuid}.json` and spaces in `{org}/spaces.json`.
/// Only `.claude/projects` is walked so large `outputs/` trees are never scanned.
fn collect_cowork_files(
    cowork_root: &Utf8Path,
    files: &mut Vec<DiscoveredFile>,
    metadata_files: &mut BTreeMap<Utf8PathBuf, (i64, i64)>,
) -> Result<(), AppError> {
    for account_dir in child_dirs(cowork_root) {
        for org_dir in child_dirs(&account_dir) {
            for (path, is_dir) in dir_entries(&org_dir) {
                if is_dir {
                    let projects_root = path.join(".claude").join("projects");
                    if projects_root.is_dir() {
                        collect_project_files(
                            &projects_root,
                            SOURCE_COWORK,
                            Some(&path),
                            files,
                            metadata_files,
                        )?;
                    }
                } else if path.file_name() == Some("spaces.json") || is_app_session_file(&path) {
                    record_metadata_file(&path, metadata_files);
                }
            }
        }
    }
    Ok(())
}

/// Session directories a Cowork metadata file may own: `local_{uuid}/`, the
/// newer `{uuid[..8]}/` layout, or whatever directory holds its `cwd`.
fn cowork_session_dirs(
    org_dir: &Utf8Path,
    meta_path: &Utf8Path,
    session: &AppSession,
) -> Vec<Utf8PathBuf> {
    let mut dirs = Vec::new();
    if let Some(stem) = meta_path.file_stem() {
        dirs.push(org_dir.join(stem));
        if let Some(uuid) = stem.strip_prefix("local_")
            && let Some(short) = uuid.get(..8)
        {
            dirs.push(org_dir.join(short));
        }
    }
    if let Some(session_dir) = session
        .cwd
        .as_ref()
        .and_then(|cwd| session_dir_for_cwd(org_dir, cwd))
    {
        dirs.push(session_dir);
    }
    dirs
}

fn session_dir_for_cwd(org_dir: &Utf8Path, cwd: &Utf8Path) -> Option<Utf8PathBuf> {
    let rel = cwd.strip_prefix(org_dir).ok()?;
    let first = rel.components().next()?;
    Some(org_dir.join(first.as_str()))
}

fn dedupe_thread_ids(files: Vec<DiscoveredFile>) -> Vec<DiscoveredFile> {
    let mut seen: BTreeMap<String, Utf8PathBuf> = BTreeMap::new();
    let mut kept = Vec::with_capacity(files.len());
    for file in files {
        if let Some(existing) = seen.get(&file.thread_id) {
            eprintln!(
                "warning: skipping duplicate thread id {} at {} (already indexed from {})",
                file.thread_id, file.path, existing
            );
            continue;
        }
        seen.insert(file.thread_id.clone(), file.path.clone());
        kept.push(file);
    }
    kept
}

struct ClassifiedFile {
    project_slug: String,
    thread_id: String,
    is_subagent: bool,
    parent_thread_id: Option<String>,
    agent_slug: Option<String>,
    workflow_id: Option<String>,
}

fn classify_file(projects_root: &Utf8Path, path: &Utf8Path) -> Option<ClassifiedFile> {
    let rel = path.strip_prefix(projects_root).ok()?;
    let mut components: Vec<&str> = rel
        .components()
        .map(|component| component.as_str())
        .collect();
    let file_name = components.pop()?.to_string();
    let stem = file_name.strip_suffix(".jsonl")?.to_string();

    let (project_slug, parent_session_id, workflow_id) = match components.as_slice() {
        [project_slug] => {
            return Some(ClassifiedFile {
                project_slug: (*project_slug).to_string(),
                thread_id: stem,
                is_subagent: false,
                parent_thread_id: None,
                agent_slug: None,
                workflow_id: None,
            });
        }
        [project_slug, parent_session_id, "subagents"] => (project_slug, parent_session_id, None),
        [
            project_slug,
            parent_session_id,
            "subagents",
            "workflows",
            workflow_id,
        ] => (
            project_slug,
            parent_session_id,
            Some((*workflow_id).to_string()),
        ),
        _ => return None,
    };
    // Requiring the `agent-` prefix also skips workflow `journal.jsonl` files.
    let (slug, hash) = parse_subagent_filename(&stem)?;
    Some(ClassifiedFile {
        project_slug: (*project_slug).to_string(),
        thread_id: format!("{parent_session_id}:agent:{hash}"),
        is_subagent: true,
        parent_thread_id: Some((*parent_session_id).to_string()),
        agent_slug: slug,
        workflow_id,
    })
}

fn parse_subagent_filename(stem: &str) -> Option<(Option<String>, String)> {
    let rest = stem.strip_prefix("agent-")?;
    if let Some((slug, hash)) = rest.rsplit_once('-') {
        Some((Some(slug.to_string()), hash.to_string()))
    } else {
        Some((None, rest.to_string()))
    }
}

fn load_history(paths: &ResolvedPaths) -> Result<BTreeMap<String, HistoryEntry>, AppError> {
    if !paths.history_path.exists() {
        return Ok(BTreeMap::new());
    }

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
    Ok(entries)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAppSession {
    session_id: Option<String>,
    cli_session_id: Option<String>,
    title: Option<String>,
    is_archived: Option<bool>,
    last_activity_at: Option<f64>,
    cwd: Option<String>,
    user_selected_folders: Option<Vec<Value>>,
    space_id: Option<String>,
}

#[derive(Deserialize)]
struct RawSpaces {
    #[serde(default)]
    spaces: Vec<RawSpace>,
}

#[derive(Deserialize)]
struct RawSpace {
    id: Option<String>,
    name: Option<String>,
    folders: Option<Vec<Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAgentMeta {
    agent_type: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCustomTitle {
    custom_title: Option<String>,
}

fn read_app_session(path: &Utf8Path) -> Option<AppSession> {
    let raw = read_json_file::<RawAppSession>(path)?;
    let app_session_id = raw
        .session_id
        .or_else(|| path.file_stem().map(ToOwned::to_owned))?;
    Some(AppSession {
        app_session_id,
        cli_session_id: non_empty(raw.cli_session_id),
        title: non_empty(raw.title),
        is_archived: raw.is_archived.unwrap_or(false),
        last_activity_at: raw.last_activity_at.map(|value| value as i64),
        space_id: non_empty(raw.space_id),
        selected_folders: folder_paths(raw.user_selected_folders),
        cwd: raw.cwd.map(Utf8PathBuf::from),
    })
}

fn load_cowork_spaces(path: &Utf8Path, spaces: &mut BTreeMap<String, CoworkSpace>) {
    let Some(raw) = read_json_file::<RawSpaces>(path) else {
        return;
    };
    for space in raw.spaces {
        let Some(id) = non_empty(space.id) else {
            continue;
        };
        spaces.insert(
            id,
            CoworkSpace {
                name: non_empty(space.name),
                folders: folder_paths(space.folders),
            },
        );
    }
}

/// Folder lists appear both as plain strings and as `{ "path": ... }` objects.
fn folder_paths(values: Option<Vec<Value>>) -> Vec<String> {
    values
        .unwrap_or_default()
        .iter()
        .filter_map(|value| {
            value
                .as_str()
                .or_else(|| value.get("path").and_then(Value::as_str))
        })
        .filter(|path| !path.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn read_json_file<T: DeserializeOwned>(path: &Utf8Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("warning: ignoring unreadable metadata file {path}: {error}");
            None
        }
    }
}

fn is_app_session_file(path: &Utf8Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.starts_with("local_") && name.ends_with(".json"))
}

fn child_dirs(dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    dir_entries(dir)
        .into_iter()
        .filter_map(|(path, is_dir)| is_dir.then_some(path))
        .collect()
}

fn dir_entries(dir: &Utf8Path) -> Vec<(Utf8PathBuf, bool)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<(Utf8PathBuf, bool)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let is_dir = entry.file_type().ok()?.is_dir();
            let path = Utf8PathBuf::from_path_buf(entry.path()).ok()?;
            Some((path, is_dir))
        })
        .collect();
    items.sort();
    items
}

fn record_metadata_file(path: &Utf8Path, metadata_files: &mut BTreeMap<Utf8PathBuf, (i64, i64)>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if !metadata.is_file() {
        return;
    }
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos() as i64)
        .unwrap_or_default();
    metadata_files.insert(path.to_path_buf(), (metadata.len() as i64, mtime_ns));
}

/// Stable FNV-1a digest over metadata file paths, sizes, and mtimes.
fn fingerprint(metadata_files: &BTreeMap<Utf8PathBuf, (i64, i64)>) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (path, (size, mtime_ns)) in metadata_files {
        feed(path.as_str().as_bytes());
        feed(&[0]);
        feed(&size.to_le_bytes());
        feed(&mtime_ns.to_le_bytes());
    }
    format!("{hash:016x}:{}", metadata_files.len())
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn extract_user_message_text(value: &Value) -> Option<String> {
    extract_content_text(value.get("message")?.get("content"))
}

/// Joins the `text` blocks of a message content value (string or block array);
/// `tool_result`, image, and document blocks are not user intent and are skipped.
fn extract_content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
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

fn extract_assistant_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    if !content.is_array() {
        return None;
    }
    extract_content_text(Some(content))
}

fn is_local_command_noise(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<local-command-stderr>")
        || trimmed.starts_with("<command-name>")
        || trimmed.starts_with("<command-message>")
        || trimmed.starts_with("<command-args>")
}

fn build_thread_search_text(
    title: Option<&str>,
    title_candidates: &[Option<String>],
    messages: &[IndexedMessage],
) -> String {
    let mut search = String::new();
    let mut seen_titles: Vec<&str> = Vec::new();
    for candidate in title
        .into_iter()
        .chain(title_candidates.iter().flatten().map(String::as_str))
    {
        if candidate.is_empty() || seen_titles.contains(&candidate) {
            continue;
        }
        seen_titles.push(candidate);
        search.push_str(candidate);
        search.push('\n');
    }
    // Harness-authored messages (role=system) stay searchable per message but
    // do not crowd the thread-level topic text.
    for message in messages
        .iter()
        .filter(|message| message.role != "system")
        .take(SEARCH_TEXT_MESSAGE_LIMIT)
    {
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
