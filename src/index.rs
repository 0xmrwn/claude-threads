use crate::archive::{
    ArchiveInventory, IndexedEvent, IndexedMessage, ParsedThread, SOURCE_COWORK, discover_archives,
    load_metadata, parse_thread, resolve_metadata,
};
use crate::error::{AppError, ErrorCode, internal, io_error};
use crate::paths::ResolvedPaths;
use camino::Utf8PathBuf;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// Bumped whenever the derived index layout changes; a mismatch rebuilds it.
const SCHEMA_VERSION: i64 = 2;

const THREAD_COLUMNS: &str = "t.thread_id, t.project_slug, t.project_cwd, t.title, t.started_at,
    t.updated_at, t.message_count, t.event_count, t.cli_version, t.git_branch, t.entrypoint,
    t.is_subagent, t.parent_thread_id, t.agent_slug, t.default_scope, t.was_compacted, t.path,
    t.source, t.project_name, t.model, t.agent_type, t.workflow_id, t.app_session_id,
    t.is_archived, t.selected_folders";

#[derive(Debug, Serialize)]
pub struct SyncSummary {
    pub discovered_files: usize,
    pub updated_files: usize,
    pub removed_files: usize,
    pub project_count: usize,
    pub thread_count: usize,
    pub subagent_count: usize,
    pub cowork_thread_count: usize,
    pub message_count: usize,
    pub event_count: usize,
    pub skipped_empty_files: usize,
    pub rebuilt: bool,
}

#[derive(Debug, Serialize)]
pub struct ThreadRecord {
    pub thread_id: String,
    pub project_slug: String,
    pub project_cwd: Option<String>,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub message_count: i64,
    pub event_count: i64,
    pub cli_version: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub is_subagent: bool,
    pub parent_thread_id: Option<String>,
    pub agent_slug: Option<String>,
    pub default_scope: bool,
    pub was_compacted: bool,
    pub path: String,
    pub source: String,
    pub project_name: Option<String>,
    pub model: Option<String>,
    pub agent_type: Option<String>,
    pub workflow_id: Option<String>,
    pub app_session_id: Option<String>,
    pub is_archived: bool,
    pub selected_folders: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ThreadSearchHit {
    pub thread_id: String,
    pub source: String,
    pub project_slug: String,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub is_subagent: bool,
    pub snippet: String,
}

#[derive(Debug, Serialize)]
pub struct MessageRecord {
    pub message_id: String,
    pub thread_id: String,
    pub source: String,
    pub project_slug: String,
    pub prompt_id: Option<String>,
    pub role: String,
    pub kind: String,
    pub timestamp: Option<String>,
    pub text: String,
    pub snippet: String,
}

#[derive(Debug, Serialize)]
pub struct MessageSearchHit {
    pub message_id: String,
    pub thread_id: String,
    pub source: String,
    pub project_slug: String,
    pub role: String,
    pub kind: String,
    pub timestamp: Option<String>,
    pub snippet: String,
}

#[derive(Debug, Serialize)]
pub struct EventRecord {
    pub event_id: String,
    pub thread_id: String,
    pub ordinal: i64,
    pub timestamp: Option<String>,
    pub record_type: String,
    pub payload: Value,
}

#[derive(Debug, Serialize)]
pub struct ProjectRecord {
    pub project_slug: String,
    pub source: String,
    pub project_name: Option<String>,
    pub project_cwd: Option<String>,
    pub thread_count: i64,
    pub last_seen_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatsRecord {
    pub index_path: String,
    pub last_sync_at: Option<String>,
    pub source_file_count: i64,
    pub project_count: i64,
    pub thread_count: i64,
    pub subagent_count: i64,
    pub cowork_thread_count: i64,
    pub message_count: i64,
    pub event_count: i64,
    pub skipped_empty_files: i64,
    pub projects_root: String,
    pub cowork_root: Option<String>,
}

#[derive(Debug, Clone)]
struct FileState {
    path: String,
    thread_id: String,
    size: i64,
    mtime_ns: i64,
    meta_json: String,
}

impl FileState {
    fn matches(&self, item: &crate::archive::DiscoveredFile) -> bool {
        self.thread_id == item.thread_id && self.size == item.size && self.mtime_ns == item.mtime_ns
    }
}

pub fn sync(paths: &ResolvedPaths, rebuild: bool) -> Result<SyncSummary, AppError> {
    let inventory = discover_archives(paths)?;
    sync_with_inventory(paths, inventory, rebuild)
}

pub fn ensure_fresh(paths: &ResolvedPaths) -> Result<bool, AppError> {
    let inventory = discover_archives(paths)?;
    if needs_sync(paths, &inventory)? {
        match sync_with_inventory(paths, inventory, false) {
            Ok(_) => return Ok(true),
            Err(error) if error.is_sqlite_locked() && paths.index_path.exists() => {
                if schema_version(&open_connection(paths, false)?)? != SCHEMA_VERSION {
                    return Err(AppError::with_details(
                        ErrorCode::IndexMissing,
                        "index schema is outdated and another process holds the sync lock; retry shortly",
                        serde_json::json!({ "index_path": paths.index_path }),
                    ));
                }
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

pub fn list_projects(
    paths: &ResolvedPaths,
    limit: usize,
) -> Result<(Vec<ProjectRecord>, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let mut stmt = conn
        .prepare(
            "SELECT project_slug, source, project_name, project_cwd, thread_count, last_seen_at
             FROM projects
             ORDER BY COALESCE(last_seen_at, '') DESC, project_slug
             LIMIT ?1",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map(params![limit as i64], |row| {
            Ok(ProjectRecord {
                project_slug: row.get(0)?,
                source: row.get(1)?,
                project_name: row.get(2)?,
                project_cwd: row.get(3)?,
                thread_count: row.get(4)?,
                last_seen_at: row.get(5)?,
            })
        })
        .map_err(sqlite_err)?;
    let items = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    Ok((items, auto_sync))
}

pub fn list_threads(
    paths: &ResolvedPaths,
    limit: usize,
    project: Option<&str>,
    source: Option<&str>,
    ascending: bool,
    include_subagents: bool,
) -> Result<(Vec<ThreadRecord>, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let resolved_project = resolve_project_filter(&conn, project)?;
    let direction = if ascending { "ASC" } else { "DESC" };

    let mut sql = format!("SELECT {THREAD_COLUMNS} FROM threads t WHERE 1 = 1");
    let mut params_vec = vec![SqlValue::Integer(limit as i64)];
    push_thread_filters(
        &mut sql,
        &mut params_vec,
        include_subagents,
        resolved_project,
        source,
    );
    sql.push_str(&format!(
        " ORDER BY CASE WHEN COALESCE(t.started_at, t.updated_at) IS NULL THEN 1 ELSE 0 END ASC,
                  COALESCE(t.started_at, t.updated_at, '') {direction},
                  t.thread_id ASC
         LIMIT ?1"
    ));

    let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            thread_record_row,
        )
        .map_err(sqlite_err)?;
    let items = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    Ok((items, auto_sync))
}

pub fn search_threads(
    paths: &ResolvedPaths,
    query: &str,
    limit: usize,
    project: Option<&str>,
    source: Option<&str>,
    include_subagents: bool,
) -> Result<(Vec<ThreadSearchHit>, bool), AppError> {
    if query.trim().is_empty() {
        return Err(AppError::new(
            ErrorCode::UsageError,
            "search query must not be empty",
        ));
    }
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let resolved_project = resolve_project_filter(&conn, project)?;
    let mut sql = String::from(
        "SELECT t.thread_id, t.source, t.project_slug, t.title, t.started_at, t.updated_at,
                t.is_subagent,
                snippet(thread_fts, 1, '', '', ' ... ', 14) AS snippet
         FROM thread_fts
         JOIN threads t USING(thread_id)
         WHERE thread_fts MATCH ?1",
    );
    let mut params_vec = vec![
        SqlValue::Text(fts_query(query)),
        SqlValue::Integer(limit as i64),
    ];
    push_thread_filters(
        &mut sql,
        &mut params_vec,
        include_subagents,
        resolved_project,
        source,
    );
    sql.push_str(
        " ORDER BY bm25(thread_fts), COALESCE(t.updated_at, '') DESC, t.thread_id LIMIT ?2",
    );

    let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            thread_search_hit_row,
        )
        .map_err(sqlite_err)?;
    let hits = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    Ok((hits, auto_sync))
}

/// Appends the shared scope/project/source filters for queries aliasing
/// `threads` as `t`, numbering placeholders after the ones already bound.
fn push_thread_filters(
    sql: &mut String,
    params_vec: &mut Vec<SqlValue>,
    include_subagents: bool,
    project_slug: Option<String>,
    source: Option<&str>,
) {
    if !include_subagents {
        sql.push_str(" AND t.default_scope = 1");
    }
    if let Some(slug) = project_slug {
        params_vec.push(SqlValue::Text(slug));
        sql.push_str(&format!(" AND t.project_slug = ?{}", params_vec.len()));
    }
    if let Some(source) = source {
        params_vec.push(SqlValue::Text(source.to_string()));
        sql.push_str(&format!(" AND t.source = ?{}", params_vec.len()));
    }
}

pub fn resolve_thread(
    paths: &ResolvedPaths,
    query: &str,
) -> Result<(ThreadRecord, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;

    if let Some(record) = read_thread_from_conn(&conn, query)? {
        return Ok((record, auto_sync));
    }

    let exact_title_matches = exact_title_matches(&conn, query)?;
    if exact_title_matches.len() == 1 {
        let record = read_thread_from_conn(&conn, &exact_title_matches[0])?
            .ok_or_else(|| internal("resolved thread disappeared"))?;
        return Ok((record, auto_sync));
    }
    if exact_title_matches.len() > 1 {
        return Err(AppError::with_details(
            ErrorCode::Ambiguous,
            format!("multiple threads exactly matched '{query}'"),
            serde_json::json!({
                "candidates": thread_hits_for_ids(&conn, &exact_title_matches)?
            }),
        ));
    }

    let candidates = search_threads_inner(&conn, query, 5)?;
    if candidates.is_empty() {
        return Err(AppError::with_details(
            ErrorCode::NotFound,
            format!("no thread matched '{query}'"),
            serde_json::json!({ "query": query }),
        ));
    }
    if candidates.len() > 1 {
        return Err(AppError::with_details(
            ErrorCode::Ambiguous,
            format!("multiple threads matched '{query}'"),
            serde_json::json!({ "candidates": candidates }),
        ));
    }
    let record = read_thread_from_conn(&conn, &candidates[0].thread_id)?
        .ok_or_else(|| internal("resolved thread disappeared"))?;
    Ok((record, auto_sync))
}

pub fn read_thread(
    paths: &ResolvedPaths,
    thread_id: &str,
) -> Result<(ThreadRecord, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let record = read_thread_from_conn(&conn, thread_id)?.ok_or_else(|| {
        AppError::with_details(
            ErrorCode::NotFound,
            format!("thread '{thread_id}' was not found"),
            serde_json::json!({ "thread_id": thread_id }),
        )
    })?;
    Ok((record, auto_sync))
}

pub fn search_messages(
    paths: &ResolvedPaths,
    query: &str,
    limit: usize,
    project: Option<&str>,
    role: Option<&str>,
    source: Option<&str>,
    include_subagents: bool,
) -> Result<(Vec<MessageSearchHit>, bool), AppError> {
    if query.trim().is_empty() {
        return Err(AppError::new(
            ErrorCode::UsageError,
            "search query must not be empty",
        ));
    }
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let resolved_project = resolve_project_filter(&conn, project)?;

    let mut sql = String::from(
        "SELECT m.message_id, m.thread_id, t.source, t.project_slug, m.role, m.kind, m.timestamp,
                snippet(message_fts, 2, '', '', ' ... ', 18) AS snippet
         FROM message_fts
         JOIN messages m USING(message_id)
         JOIN threads t ON t.thread_id = m.thread_id
         WHERE message_fts MATCH ?1",
    );
    let mut params_vec = vec![
        SqlValue::Text(fts_query(query)),
        SqlValue::Integer(limit as i64),
    ];
    push_thread_filters(
        &mut sql,
        &mut params_vec,
        include_subagents,
        resolved_project,
        source,
    );
    if let Some(role) = role {
        params_vec.push(SqlValue::Text(role.to_string()));
        sql.push_str(&format!(" AND m.role = ?{}", params_vec.len()));
    }
    sql.push_str(
        " ORDER BY bm25(message_fts), COALESCE(m.timestamp, '') DESC, m.message_id LIMIT ?2",
    );

    let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params_vec.iter()), |row| {
            Ok(MessageSearchHit {
                message_id: row.get(0)?,
                thread_id: row.get(1)?,
                source: row.get(2)?,
                project_slug: row.get(3)?,
                role: row.get(4)?,
                kind: row.get(5)?,
                timestamp: row.get(6)?,
                snippet: row.get(7)?,
            })
        })
        .map_err(sqlite_err)?;
    let hits = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    Ok((hits, auto_sync))
}

pub fn list_messages(
    paths: &ResolvedPaths,
    limit: usize,
    project: Option<&str>,
    role: Option<&str>,
    source: Option<&str>,
    ascending: bool,
    include_subagents: bool,
) -> Result<(Vec<MessageRecord>, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let resolved_project = resolve_project_filter(&conn, project)?;
    let direction = if ascending { "ASC" } else { "DESC" };

    let mut sql = String::from(
        "SELECT m.message_id, m.thread_id, t.source, t.project_slug, m.prompt_id, m.role, m.kind,
                m.timestamp, m.text, m.snippet
         FROM messages m
         JOIN threads t ON t.thread_id = m.thread_id
         WHERE 1 = 1",
    );
    let mut params_vec = vec![SqlValue::Integer(limit as i64)];
    push_thread_filters(
        &mut sql,
        &mut params_vec,
        include_subagents,
        resolved_project,
        source,
    );
    if let Some(role) = role {
        params_vec.push(SqlValue::Text(role.to_string()));
        sql.push_str(&format!(" AND m.role = ?{}", params_vec.len()));
    }
    sql.push_str(&format!(
        " ORDER BY CASE WHEN m.timestamp IS NULL THEN 1 ELSE 0 END ASC,
                  COALESCE(m.timestamp, '') {direction},
                  m.thread_id ASC,
                  m.ordinal ASC
         LIMIT ?1"
    ));

    let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            message_record_row,
        )
        .map_err(sqlite_err)?;
    let items = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    Ok((items, auto_sync))
}

pub fn read_message(
    paths: &ResolvedPaths,
    message_id: &str,
) -> Result<(MessageRecord, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let mut stmt = conn
        .prepare(
            "SELECT m.message_id, m.thread_id, t.source, t.project_slug, m.prompt_id, m.role,
                    m.kind, m.timestamp, m.text, m.snippet
             FROM messages m
             JOIN threads t ON t.thread_id = m.thread_id
             WHERE m.message_id = ?1",
        )
        .map_err(sqlite_err)?;
    let record = stmt
        .query_row([message_id], message_record_row)
        .optional()
        .map_err(sqlite_err)?
        .ok_or_else(|| {
            AppError::with_details(
                ErrorCode::NotFound,
                format!("message '{message_id}' was not found"),
                serde_json::json!({ "message_id": message_id }),
            )
        })?;
    Ok((record, auto_sync))
}

pub fn read_events(
    paths: &ResolvedPaths,
    thread_id: &str,
    limit: usize,
) -> Result<(Vec<EventRecord>, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let mut stmt = conn
        .prepare(
            "SELECT event_id, thread_id, ordinal, timestamp, record_type,
                    file_path, byte_start, byte_len
             FROM events
             WHERE thread_id = ?1
             ORDER BY ordinal
             LIMIT ?2",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map(params![thread_id, limit as i64], |row| {
            Ok(IndexedEvent {
                event_id: row.get(0)?,
                thread_id: row.get(1)?,
                ordinal: row.get(2)?,
                timestamp: row.get(3)?,
                record_type: row.get(4)?,
                file_path: Utf8PathBuf::from(row.get::<_, String>(5)?),
                byte_start: row.get(6)?,
                byte_len: row.get(7)?,
            })
        })
        .map_err(sqlite_err)?;
    let indexed = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    if indexed.is_empty() {
        return Err(AppError::with_details(
            ErrorCode::NotFound,
            format!("thread '{thread_id}' was not found"),
            serde_json::json!({ "thread_id": thread_id }),
        ));
    }
    let file_path = indexed[0].file_path.clone();
    let mut file = File::open(&file_path)
        .map_err(|error| io_error(&format!("failed to open {file_path}"), error))?;
    let mut records = Vec::with_capacity(indexed.len());
    for event in indexed {
        let payload = read_payload(&mut file, &event)?;
        records.push(EventRecord {
            event_id: event.event_id,
            thread_id: event.thread_id,
            ordinal: event.ordinal,
            timestamp: event.timestamp,
            record_type: event.record_type,
            payload,
        });
    }
    Ok((records, auto_sync))
}

pub fn stats(paths: &ResolvedPaths) -> Result<(StatsRecord, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let last_sync_at = read_state(&conn, "last_sync_at")?;
    let counts = IndexCounts::load(&conn)?;
    let source_file_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
        .map_err(sqlite_err)?;

    Ok((
        StatsRecord {
            index_path: paths.index_path.to_string(),
            last_sync_at,
            source_file_count,
            project_count: counts.projects,
            thread_count: counts.threads,
            subagent_count: counts.subagents,
            cowork_thread_count: counts.cowork_threads,
            message_count: counts.messages,
            event_count: counts.events,
            skipped_empty_files: counts.empty_files,
            projects_root: paths.projects_root.to_string(),
            cowork_root: paths.cowork_root.as_ref().map(ToString::to_string),
        },
        auto_sync,
    ))
}

struct IndexCounts {
    projects: i64,
    threads: i64,
    subagents: i64,
    cowork_threads: i64,
    messages: i64,
    events: i64,
    empty_files: i64,
}

impl IndexCounts {
    fn load(conn: &Connection) -> Result<Self, AppError> {
        let count = |sql: &str| -> Result<i64, AppError> {
            conn.query_row(sql, [], |row| row.get(0))
                .map_err(sqlite_err)
        };
        Ok(Self {
            projects: count("SELECT COUNT(*) FROM projects")?,
            threads: count("SELECT COUNT(*) FROM threads WHERE is_subagent = 0")?,
            subagents: count("SELECT COUNT(*) FROM threads WHERE is_subagent = 1")?,
            cowork_threads: conn
                .query_row(
                    "SELECT COUNT(*) FROM threads WHERE is_subagent = 0 AND source = ?1",
                    [SOURCE_COWORK],
                    |row| row.get(0),
                )
                .map_err(sqlite_err)?,
            messages: count("SELECT COUNT(*) FROM messages")?,
            events: count("SELECT COUNT(*) FROM events")?,
            empty_files: count("SELECT COUNT(*) FROM files WHERE indexed = 0")?,
        })
    }
}

fn needs_sync(paths: &ResolvedPaths, inventory: &ArchiveInventory) -> Result<bool, AppError> {
    if !paths.index_path.exists() {
        return Ok(true);
    }

    let conn = open_connection(paths, false)?;
    if schema_version(&conn)? != SCHEMA_VERSION {
        return Ok(true);
    }
    let db_files = load_file_state(&conn)?;
    if db_files.len() != inventory.files.len() {
        return Ok(true);
    }
    for item in &inventory.files {
        match db_files.get(item.path.as_str()) {
            Some(state) if state.matches(item) => {}
            _ => return Ok(true),
        }
    }

    let indexed_fingerprint = read_state(&conn, "metadata_fingerprint")?;
    Ok(indexed_fingerprint.as_deref() != Some(inventory.metadata_fingerprint.as_str()))
}

fn sync_with_inventory(
    paths: &ResolvedPaths,
    inventory: ArchiveInventory,
    rebuild: bool,
) -> Result<SyncSummary, AppError> {
    for (skipped, kept) in &inventory.duplicate_files {
        eprintln!(
            "warning: skipping duplicate thread id at {skipped} (already indexed from {kept})"
        );
    }
    paths.ensure_index_dir()?;
    let mut conn = open_connection(paths, true)?;
    let schema_reset = init_schema(&mut conn)?;
    if rebuild {
        clear_all(&conn)?;
    }

    let existing = load_file_state(&conn)?;
    // Metadata is only loaded (and every thread re-resolved) when some
    // enrichment file changed; otherwise only changed transcripts are parsed.
    let metadata_changed = rebuild
        || schema_reset
        || read_state(&conn, "metadata_fingerprint")?.as_deref()
            != Some(inventory.metadata_fingerprint.as_str());
    let current_paths = inventory
        .files
        .iter()
        .map(|item| item.path.as_str())
        .collect::<BTreeSet<_>>();
    let removed = existing
        .keys()
        .filter(|path| !current_paths.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    let mut metadata = None;
    let mut updated = Vec::new();
    for item in &inventory.files {
        let state = existing.get(item.path.as_str());
        let transcript_changed = !state.is_some_and(|state| state.matches(item));
        if !transcript_changed && !metadata_changed {
            continue;
        }
        if metadata.is_none() {
            metadata = Some(load_metadata(paths, &inventory)?);
        }
        let loaded = metadata.as_ref().expect("metadata loaded above");
        let meta = resolve_metadata(loaded, &inventory, item);
        let meta_json = serde_json::to_string(&meta)
            .map_err(|error| internal(format!("failed to encode thread metadata: {error}")))?;
        if !transcript_changed && state.is_some_and(|state| state.meta_json == meta_json) {
            continue;
        }
        updated.push((item, meta, meta_json));
    }

    let transaction = conn.transaction().map_err(sqlite_err)?;
    for path in &removed {
        delete_file_records(&transaction, path)?;
    }
    for (item, meta, meta_json) in &updated {
        delete_thread_records(&transaction, &item.thread_id)?;
        let parsed = parse_thread(item, meta)?;
        insert_parsed_thread(&transaction, item, &parsed, meta_json)?;
    }
    refresh_projects(&transaction)?;
    update_state(&transaction, &inventory)?;
    transaction.commit().map_err(sqlite_err)?;

    let counts = IndexCounts::load(&conn)?;
    Ok(SyncSummary {
        discovered_files: inventory.files.len(),
        updated_files: updated.len(),
        removed_files: removed.len(),
        project_count: counts.projects as usize,
        thread_count: counts.threads as usize,
        subagent_count: counts.subagents as usize,
        cowork_thread_count: counts.cowork_threads as usize,
        message_count: counts.messages as usize,
        event_count: counts.events as usize,
        skipped_empty_files: counts.empty_files as usize,
        rebuilt: rebuild || schema_reset,
    })
}

fn open_connection(paths: &ResolvedPaths, create_dirs: bool) -> Result<Connection, AppError> {
    if create_dirs {
        paths.ensure_index_dir()?;
    }
    let conn = Connection::open(&paths.index_path).map_err(sqlite_err)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(sqlite_err)?;
    Ok(conn)
}

fn schema_version(conn: &Connection) -> Result<i64, AppError> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(sqlite_err)
}

fn read_state(conn: &Connection, key: &str) -> Result<Option<String>, AppError> {
    conn.query_row("SELECT value FROM state WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
    .map_err(sqlite_err)
}

/// Creates the index tables, dropping any layout from another schema version.
/// Returns true when the index was reset and must be fully rebuilt.
fn init_schema(conn: &mut Connection) -> Result<bool, AppError> {
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
        .map_err(sqlite_err)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sqlite_err)?;
    let reset = schema_version(&tx)? != SCHEMA_VERSION;
    if reset {
        tx.execute_batch(
            "
            DROP TABLE IF EXISTS state;
            DROP TABLE IF EXISTS files;
            DROP TABLE IF EXISTS projects;
            DROP TABLE IF EXISTS threads;
            DROP TABLE IF EXISTS messages;
            DROP TABLE IF EXISTS events;
            DROP TABLE IF EXISTS thread_fts;
            DROP TABLE IF EXISTS message_fts;
            ",
        )
        .map_err(sqlite_err)?;
    }
    tx.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS state (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS files (
            path TEXT PRIMARY KEY,
            thread_id TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            meta_json TEXT NOT NULL,
            indexed INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS projects (
            project_slug TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            project_name TEXT,
            project_cwd TEXT,
            thread_count INTEGER NOT NULL,
            last_seen_at TEXT
        );
        CREATE TABLE IF NOT EXISTS threads (
            thread_id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            project_slug TEXT NOT NULL,
            project_name TEXT,
            project_cwd TEXT,
            path TEXT NOT NULL,
            is_subagent INTEGER NOT NULL,
            parent_thread_id TEXT,
            agent_slug TEXT,
            agent_type TEXT,
            workflow_id TEXT,
            default_scope INTEGER NOT NULL,
            title TEXT,
            started_at TEXT,
            updated_at TEXT,
            message_count INTEGER NOT NULL,
            event_count INTEGER NOT NULL,
            cli_version TEXT,
            git_branch TEXT,
            entrypoint TEXT,
            model TEXT,
            app_session_id TEXT,
            is_archived INTEGER NOT NULL,
            selected_folders TEXT NOT NULL,
            was_compacted INTEGER NOT NULL,
            search_text TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            message_id TEXT PRIMARY KEY,
            thread_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            prompt_id TEXT,
            role TEXT NOT NULL,
            kind TEXT NOT NULL,
            timestamp TEXT,
            text TEXT NOT NULL,
            snippet TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS events (
            event_id TEXT PRIMARY KEY,
            thread_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            timestamp TEXT,
            record_type TEXT NOT NULL,
            file_path TEXT NOT NULL,
            byte_start INTEGER NOT NULL,
            byte_len INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_files_thread_id ON files(thread_id);
        CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id, ordinal);
        CREATE INDEX IF NOT EXISTS idx_events_thread_id ON events(thread_id, ordinal);
        CREATE INDEX IF NOT EXISTS idx_threads_project ON threads(project_slug, updated_at DESC);
        CREATE VIRTUAL TABLE IF NOT EXISTS thread_fts USING fts5(
            thread_id UNINDEXED,
            title,
            search_text
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
            message_id UNINDEXED,
            thread_id UNINDEXED,
            text
        );
        ",
    )
    .map_err(sqlite_err)?;
    if reset {
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
            .map_err(sqlite_err)?;
    }
    tx.commit().map_err(sqlite_err)?;
    Ok(reset)
}

fn clear_all(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "
        DELETE FROM state;
        DELETE FROM files;
        DELETE FROM projects;
        DELETE FROM threads;
        DELETE FROM messages;
        DELETE FROM events;
        DELETE FROM thread_fts;
        DELETE FROM message_fts;
        ",
    )
    .map_err(sqlite_err)
}

fn load_file_state(conn: &Connection) -> Result<BTreeMap<String, FileState>, AppError> {
    let mut stmt = conn
        .prepare("SELECT path, thread_id, size, mtime_ns, meta_json FROM files")
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(FileState {
                path: row.get(0)?,
                thread_id: row.get(1)?,
                size: row.get(2)?,
                mtime_ns: row.get(3)?,
                meta_json: row.get(4)?,
            })
        })
        .map_err(sqlite_err)?;
    let states = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    Ok(states
        .into_iter()
        .map(|state| (state.path.clone(), state))
        .collect())
}

fn delete_file_records(tx: &Transaction<'_>, path: &str) -> Result<(), AppError> {
    let thread_id: Option<String> = tx
        .query_row(
            "SELECT thread_id FROM files WHERE path = ?1",
            [path],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_err)?;
    if let Some(thread_id) = thread_id {
        delete_thread_records(tx, &thread_id)?;
    }
    tx.execute("DELETE FROM files WHERE path = ?1", [path])
        .map_err(sqlite_err)?;
    Ok(())
}

fn delete_thread_records(tx: &Transaction<'_>, thread_id: &str) -> Result<(), AppError> {
    tx.execute("DELETE FROM message_fts WHERE thread_id = ?1", [thread_id])
        .map_err(sqlite_err)?;
    tx.execute("DELETE FROM thread_fts WHERE thread_id = ?1", [thread_id])
        .map_err(sqlite_err)?;
    tx.execute("DELETE FROM messages WHERE thread_id = ?1", [thread_id])
        .map_err(sqlite_err)?;
    tx.execute("DELETE FROM events WHERE thread_id = ?1", [thread_id])
        .map_err(sqlite_err)?;
    tx.execute("DELETE FROM threads WHERE thread_id = ?1", [thread_id])
        .map_err(sqlite_err)?;
    tx.execute("DELETE FROM files WHERE thread_id = ?1", [thread_id])
        .map_err(sqlite_err)?;
    Ok(())
}

fn insert_parsed_thread(
    tx: &Transaction<'_>,
    file: &crate::archive::DiscoveredFile,
    parsed: &ParsedThread,
    meta_json: &str,
) -> Result<(), AppError> {
    let thread = &parsed.thread;
    tx.execute(
        "INSERT INTO files(path, thread_id, size, mtime_ns, meta_json, indexed)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            file.path.as_str(),
            file.thread_id,
            file.size,
            file.mtime_ns,
            meta_json,
            bool_to_i64(!parsed.is_empty)
        ],
    )
    .map_err(sqlite_err)?;
    // Files without any conversation records are tracked for freshness only.
    if parsed.is_empty {
        return Ok(());
    }
    let selected_folders = serde_json::to_string(&thread.selected_folders)
        .map_err(|error| internal(format!("failed to encode selected folders: {error}")))?;
    tx.execute(
        "INSERT INTO threads(
            thread_id, source, project_slug, project_name, project_cwd, path, is_subagent,
            parent_thread_id, agent_slug, agent_type, workflow_id, default_scope, title,
            started_at, updated_at, message_count, event_count, cli_version, git_branch,
            entrypoint, model, app_session_id, is_archived, selected_folders, was_compacted,
            search_text
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                  ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
        params![
            thread.thread_id,
            thread.source,
            thread.project_slug,
            thread.project_name,
            thread.project_cwd,
            thread.path.as_str(),
            bool_to_i64(thread.is_subagent),
            thread.parent_thread_id,
            thread.agent_slug,
            thread.agent_type,
            thread.workflow_id,
            bool_to_i64(thread.default_scope),
            thread.title,
            thread.started_at,
            thread.updated_at,
            thread.message_count,
            thread.event_count,
            thread.cli_version,
            thread.git_branch,
            thread.entrypoint,
            thread.model,
            thread.app_session_id,
            bool_to_i64(thread.is_archived),
            selected_folders,
            bool_to_i64(thread.was_compacted),
            thread.search_text
        ],
    )
    .map_err(sqlite_err)?;
    tx.execute(
        "INSERT INTO thread_fts(thread_id, title, search_text) VALUES(?1, ?2, ?3)",
        params![thread.thread_id, thread.title, thread.search_text],
    )
    .map_err(sqlite_err)?;
    insert_messages(tx, &parsed.messages)?;
    insert_events(tx, &parsed.events)?;
    Ok(())
}

fn insert_messages(tx: &Transaction<'_>, messages: &[IndexedMessage]) -> Result<(), AppError> {
    let mut insert = tx
        .prepare(
            "INSERT INTO messages(
                message_id, thread_id, ordinal, prompt_id, role, kind, timestamp, text, snippet
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .map_err(sqlite_err)?;
    let mut insert_fts = tx
        .prepare("INSERT INTO message_fts(message_id, thread_id, text) VALUES(?1, ?2, ?3)")
        .map_err(sqlite_err)?;
    for message in messages {
        insert
            .execute(params![
                message.message_id,
                message.thread_id,
                message.ordinal,
                message.prompt_id,
                message.role,
                message.kind,
                message.timestamp,
                message.text,
                message.snippet
            ])
            .map_err(sqlite_err)?;
        insert_fts
            .execute(params![message.message_id, message.thread_id, message.text])
            .map_err(sqlite_err)?;
    }
    Ok(())
}

fn insert_events(tx: &Transaction<'_>, events: &[IndexedEvent]) -> Result<(), AppError> {
    let mut insert = tx
        .prepare(
            "INSERT INTO events(
                event_id, thread_id, ordinal, timestamp, record_type,
                file_path, byte_start, byte_len
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .map_err(sqlite_err)?;
    for event in events {
        insert
            .execute(params![
                event.event_id,
                event.thread_id,
                event.ordinal,
                event.timestamp,
                event.record_type,
                event.file_path.as_str(),
                event.byte_start,
                event.byte_len
            ])
            .map_err(sqlite_err)?;
    }
    Ok(())
}

fn refresh_projects(tx: &Transaction<'_>) -> Result<(), AppError> {
    tx.execute("DELETE FROM projects", []).map_err(sqlite_err)?;
    // The catch-all Cowork project spans unrelated folders, so it has no cwd.
    tx.execute(
        "INSERT INTO projects(
            project_slug, source, project_name, project_cwd, thread_count, last_seen_at
         )
         SELECT project_slug,
                MAX(source),
                MAX(project_name),
                CASE WHEN project_slug = ?1 THEN NULL ELSE MAX(project_cwd) END,
                COUNT(*) FILTER (WHERE is_subagent = 0),
                MAX(updated_at)
         FROM threads
         GROUP BY project_slug",
        [crate::archive::COWORK_PROJECT_SLUG],
    )
    .map_err(sqlite_err)?;
    Ok(())
}

fn update_state(tx: &Transaction<'_>, inventory: &ArchiveInventory) -> Result<(), AppError> {
    let now = crate::envelope::now_rfc3339();
    tx.execute(
        "INSERT INTO state(key, value) VALUES('last_sync_at', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [now],
    )
    .map_err(sqlite_err)?;
    tx.execute(
        "INSERT INTO state(key, value) VALUES('metadata_fingerprint', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [inventory.metadata_fingerprint.as_str()],
    )
    .map_err(sqlite_err)?;
    Ok(())
}

fn exact_title_matches(conn: &Connection, query: &str) -> Result<Vec<String>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT thread_id FROM threads
             WHERE default_scope = 1
               AND lower(COALESCE(title, '')) = lower(?1)
             ORDER BY COALESCE(updated_at, '') DESC, thread_id",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map([query], |row| row.get(0))
        .map_err(sqlite_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)
}

fn thread_hits_for_ids(
    conn: &Connection,
    thread_ids: &[String],
) -> Result<Vec<ThreadSearchHit>, AppError> {
    let mut hits = Vec::with_capacity(thread_ids.len());
    let mut stmt = conn
        .prepare(
            "SELECT thread_id, source, project_slug, title, started_at, updated_at, is_subagent
             FROM threads
             WHERE thread_id = ?1",
        )
        .map_err(sqlite_err)?;
    for thread_id in thread_ids {
        let hit = stmt
            .query_row([thread_id], |row| {
                let title: Option<String> = row.get(3)?;
                Ok(ThreadSearchHit {
                    thread_id: row.get(0)?,
                    source: row.get(1)?,
                    project_slug: row.get(2)?,
                    title: title.clone(),
                    started_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    is_subagent: row.get::<_, i64>(6)? != 0,
                    snippet: title.unwrap_or_default(),
                })
            })
            .optional()
            .map_err(sqlite_err)?;
        if let Some(hit) = hit {
            hits.push(hit);
        }
    }
    Ok(hits)
}

fn search_threads_inner(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<ThreadSearchHit>, AppError> {
    let fts = fts_query(query);
    let mut stmt = conn
        .prepare(
            "SELECT t.thread_id, t.source, t.project_slug, t.title, t.started_at, t.updated_at,
                    t.is_subagent,
                    snippet(thread_fts, 1, '', '', ' ... ', 14) AS snippet
             FROM thread_fts
             JOIN threads t USING(thread_id)
             WHERE thread_fts MATCH ?1 AND t.default_scope = 1
             ORDER BY bm25(thread_fts), COALESCE(t.updated_at, '') DESC, t.thread_id
             LIMIT ?2",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map(params![fts, limit as i64], thread_search_hit_row)
        .map_err(sqlite_err)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)
}

fn thread_search_hit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadSearchHit> {
    Ok(ThreadSearchHit {
        thread_id: row.get(0)?,
        source: row.get(1)?,
        project_slug: row.get(2)?,
        title: row.get(3)?,
        started_at: row.get(4)?,
        updated_at: row.get(5)?,
        is_subagent: row.get::<_, i64>(6)? != 0,
        snippet: row.get(7)?,
    })
}

/// Maps a row selected with `THREAD_COLUMNS`.
fn thread_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadRecord> {
    let selected_folders: String = row.get(24)?;
    Ok(ThreadRecord {
        thread_id: row.get(0)?,
        project_slug: row.get(1)?,
        project_cwd: row.get(2)?,
        title: row.get(3)?,
        started_at: row.get(4)?,
        updated_at: row.get(5)?,
        message_count: row.get(6)?,
        event_count: row.get(7)?,
        cli_version: row.get(8)?,
        git_branch: row.get(9)?,
        entrypoint: row.get(10)?,
        is_subagent: row.get::<_, i64>(11)? != 0,
        parent_thread_id: row.get(12)?,
        agent_slug: row.get(13)?,
        default_scope: row.get::<_, i64>(14)? != 0,
        was_compacted: row.get::<_, i64>(15)? != 0,
        path: row.get(16)?,
        source: row.get(17)?,
        project_name: row.get(18)?,
        model: row.get(19)?,
        agent_type: row.get(20)?,
        workflow_id: row.get(21)?,
        app_session_id: row.get(22)?,
        is_archived: row.get::<_, i64>(23)? != 0,
        selected_folders: serde_json::from_str(&selected_folders).unwrap_or_default(),
    })
}

fn message_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
    Ok(MessageRecord {
        message_id: row.get(0)?,
        thread_id: row.get(1)?,
        source: row.get(2)?,
        project_slug: row.get(3)?,
        prompt_id: row.get(4)?,
        role: row.get(5)?,
        kind: row.get(6)?,
        timestamp: row.get(7)?,
        text: row.get(8)?,
        snippet: row.get(9)?,
    })
}

fn read_thread_from_conn(
    conn: &Connection,
    thread_id: &str,
) -> Result<Option<ThreadRecord>, AppError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {THREAD_COLUMNS} FROM threads t WHERE t.thread_id = ?1"
        ))
        .map_err(sqlite_err)?;
    stmt.query_row([thread_id], thread_record_row)
        .optional()
        .map_err(sqlite_err)
}

/// Resolves `--project` by exact slug, exact cwd, case-insensitive display
/// name, then a unique case-insensitive substring of any of those. Matching
/// runs in Rust so non-ASCII names fold correctly and `%`/`_` stay literal.
fn resolve_project_filter(
    conn: &Connection,
    project: Option<&str>,
) -> Result<Option<String>, AppError> {
    let Some(query) = project else {
        return Ok(None);
    };
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    struct Candidate {
        slug: String,
        cwd: Option<String>,
        name: Option<String>,
    }
    let mut stmt = conn
        .prepare(
            "SELECT project_slug, project_cwd, project_name FROM projects
             ORDER BY thread_count DESC, project_slug",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Candidate {
                slug: row.get(0)?,
                cwd: row.get(1)?,
                name: row.get(2)?,
            })
        })
        .map_err(sqlite_err)?;
    let projects = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;

    let folded = trimmed.to_lowercase();
    let exact = projects
        .iter()
        .find(|project| project.slug == trimmed)
        .or_else(|| {
            projects
                .iter()
                .find(|project| project.cwd.as_deref() == Some(trimmed))
        })
        .or_else(|| {
            projects.iter().find(|project| {
                project
                    .name
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase() == folded)
            })
        });
    if let Some(project) = exact {
        return Ok(Some(project.slug.clone()));
    }

    let mut matches = projects
        .iter()
        .filter(|project| {
            [
                Some(project.slug.as_str()),
                project.cwd.as_deref(),
                project.name.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|field| field.to_lowercase().contains(&folded))
        })
        .map(|project| project.slug.clone())
        .take(5)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(AppError::with_details(
            ErrorCode::NotFound,
            format!("no project matched '{trimmed}'"),
            serde_json::json!({ "project": trimmed }),
        ));
    }
    if matches.len() > 1 {
        return Err(AppError::with_details(
            ErrorCode::Ambiguous,
            format!("multiple projects matched '{trimmed}'"),
            serde_json::json!({ "candidates": matches }),
        ));
    }
    Ok(Some(matches.remove(0)))
}

fn read_payload(file: &mut File, event: &IndexedEvent) -> Result<Value, AppError> {
    file.seek(SeekFrom::Start(event.byte_start as u64))
        .map_err(|error| io_error("failed to seek within event source file", error))?;
    let mut buffer = vec![0_u8; event.byte_len as usize];
    file.read_exact(&mut buffer)
        .map_err(|error| io_error("failed to read event source bytes", error))?;
    let line = String::from_utf8(buffer)
        .map_err(|error| internal(format!("event bytes were not valid UTF-8: {error}")))?;
    let value: Value = serde_json::from_str(line.trim_end())
        .map_err(|error| internal(format!("failed to re-parse indexed event: {error}")))?;
    Ok(value)
}

fn sqlite_err(error: rusqlite::Error) -> AppError {
    AppError::with_details(
        ErrorCode::InternalError,
        format!("sqlite error: {error}"),
        serde_json::json!({ "sqlite_error": error.to_string() }),
    )
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn fts_query(input: &str) -> String {
    let terms = input
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() {
        format!("\"{}\"", input.replace('"', "\"\""))
    } else {
        terms.join(" ")
    }
}
