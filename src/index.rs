use crate::archive::{
    ArchiveInventory, IndexedEvent, IndexedMessage, ParsedThread, discover_archives, parse_thread,
};
use crate::error::{AppError, ErrorCode, internal, io_error};
use crate::paths::ResolvedPaths;
use camino::Utf8PathBuf;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Serialize)]
pub struct SyncSummary {
    pub discovered_files: usize,
    pub updated_files: usize,
    pub removed_files: usize,
    pub project_count: usize,
    pub thread_count: usize,
    pub subagent_count: usize,
    pub message_count: usize,
    pub event_count: usize,
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
}

#[derive(Debug, Serialize)]
pub struct ThreadSearchHit {
    pub thread_id: String,
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
    pub message_count: i64,
    pub event_count: i64,
    pub projects_root: String,
}

#[derive(Debug, Clone)]
struct FileState {
    path: String,
    thread_id: String,
    size: i64,
    mtime_ns: i64,
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
            "SELECT project_slug, project_cwd, thread_count, last_seen_at
             FROM projects
             ORDER BY COALESCE(last_seen_at, '') DESC, project_slug
             LIMIT ?1",
        )
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map(params![limit as i64], |row| {
            Ok(ProjectRecord {
                project_slug: row.get(0)?,
                project_cwd: row.get(1)?,
                thread_count: row.get(2)?,
                last_seen_at: row.get(3)?,
            })
        })
        .map_err(sqlite_err)?;
    let items = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    Ok((items, auto_sync))
}

pub fn search_threads(
    paths: &ResolvedPaths,
    query: &str,
    limit: usize,
    project: Option<&str>,
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
    let fts = fts_query(query);
    let mut sql = String::from(
        "SELECT t.thread_id, t.project_slug, t.title, t.started_at, t.updated_at,
                t.is_subagent,
                snippet(thread_fts, 1, '', '', ' ... ', 14) AS snippet
         FROM thread_fts
         JOIN threads t USING(thread_id)
         WHERE thread_fts MATCH ?1",
    );
    if !include_subagents {
        sql.push_str(" AND t.default_scope = 1");
    }
    if resolved_project.is_some() {
        sql.push_str(" AND t.project_slug = ?3");
    }
    sql.push_str(
        " ORDER BY bm25(thread_fts), COALESCE(t.updated_at, '') DESC, t.thread_id LIMIT ?2",
    );

    let hits = if let Some(slug) = resolved_project {
        let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
        let rows = stmt
            .query_map(params![fts, limit as i64, slug], thread_search_hit_row)
            .map_err(sqlite_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?
    } else {
        let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
        let rows = stmt
            .query_map(params![fts, limit as i64], thread_search_hit_row)
            .map_err(sqlite_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?
    };
    Ok((hits, auto_sync))
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
    let fts = fts_query(query);

    let mut sql = String::from(
        "SELECT m.message_id, m.thread_id, t.project_slug, m.role, m.kind, m.timestamp,
                snippet(message_fts, 2, '', '', ' ... ', 18) AS snippet
         FROM message_fts
         JOIN messages m USING(message_id)
         JOIN threads t ON t.thread_id = m.thread_id
         WHERE message_fts MATCH ?1",
    );
    if !include_subagents {
        sql.push_str(" AND t.default_scope = 1");
    }
    let mut next_param = 3;
    if resolved_project.is_some() {
        sql.push_str(&format!(" AND t.project_slug = ?{next_param}"));
        next_param += 1;
    }
    if role.is_some() {
        sql.push_str(&format!(" AND m.role = ?{next_param}"));
    }
    sql.push_str(
        " ORDER BY bm25(message_fts), COALESCE(m.timestamp, '') DESC, m.message_id LIMIT ?2",
    );

    let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
    let mut params_vec: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Text(fts),
        rusqlite::types::Value::Integer(limit as i64),
    ];
    if let Some(slug) = resolved_project {
        params_vec.push(rusqlite::types::Value::Text(slug));
    }
    if let Some(role) = role {
        params_vec.push(rusqlite::types::Value::Text(role.to_string()));
    }
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params_vec.iter()), |row| {
            Ok(MessageSearchHit {
                message_id: row.get(0)?,
                thread_id: row.get(1)?,
                project_slug: row.get(2)?,
                role: row.get(3)?,
                kind: row.get(4)?,
                timestamp: row.get(5)?,
                snippet: row.get(6)?,
            })
        })
        .map_err(sqlite_err)?;
    let hits = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
    Ok((hits, auto_sync))
}

pub fn read_message(
    paths: &ResolvedPaths,
    message_id: &str,
) -> Result<(MessageRecord, bool), AppError> {
    let auto_sync = ensure_fresh(paths)?;
    let conn = open_connection(paths, false)?;
    let mut stmt = conn
        .prepare(
            "SELECT m.message_id, m.thread_id, t.project_slug, m.prompt_id, m.role, m.kind,
                    m.timestamp, m.text, m.snippet
             FROM messages m
             JOIN threads t ON t.thread_id = m.thread_id
             WHERE m.message_id = ?1",
        )
        .map_err(sqlite_err)?;
    let record = stmt
        .query_row([message_id], |row| {
            Ok(MessageRecord {
                message_id: row.get(0)?,
                thread_id: row.get(1)?,
                project_slug: row.get(2)?,
                prompt_id: row.get(3)?,
                role: row.get(4)?,
                kind: row.get(5)?,
                timestamp: row.get(6)?,
                text: row.get(7)?,
                snippet: row.get(8)?,
            })
        })
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
    let last_sync_at: Option<String> = conn
        .query_row(
            "SELECT value FROM state WHERE key = 'last_sync_at'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_err)?;
    let source_file_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
        .map_err(sqlite_err)?;
    let project_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
        .map_err(sqlite_err)?;
    let thread_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE is_subagent = 0",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_err)?;
    let subagent_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE is_subagent = 1",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_err)?;
    let message_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .map_err(sqlite_err)?;
    let event_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .map_err(sqlite_err)?;

    Ok((
        StatsRecord {
            index_path: paths.index_path.to_string(),
            last_sync_at,
            source_file_count,
            project_count,
            thread_count,
            subagent_count,
            message_count,
            event_count,
            projects_root: paths.projects_root.to_string(),
        },
        auto_sync,
    ))
}

fn needs_sync(paths: &ResolvedPaths, inventory: &ArchiveInventory) -> Result<bool, AppError> {
    if !paths.index_path.exists() {
        return Ok(true);
    }

    let conn = open_connection(paths, false)?;
    let db_files = load_file_state(&conn)?;
    if db_files.len() != inventory.files.len() {
        return Ok(true);
    }
    let current_paths = inventory
        .files
        .iter()
        .map(|item| item.path.as_str())
        .collect::<BTreeSet<_>>();
    let indexed_paths = db_files.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if current_paths != indexed_paths {
        return Ok(true);
    }
    for item in &inventory.files {
        let Some(state) = db_files.get(item.path.as_str()) else {
            return Ok(true);
        };
        if state.thread_id != item.thread_id
            || state.size != item.size
            || state.mtime_ns != item.mtime_ns
        {
            return Ok(true);
        }
    }

    let indexed_history_mtime_ns: Option<i64> = conn
        .query_row(
            "SELECT value FROM state WHERE key = 'history_mtime_ns'",
            [],
            |row| {
                let value: String = row.get(0)?;
                Ok(value.parse::<i64>().ok())
            },
        )
        .optional()
        .map_err(sqlite_err)?
        .flatten();
    Ok(indexed_history_mtime_ns != inventory.history_mtime_ns)
}

fn sync_with_inventory(
    paths: &ResolvedPaths,
    inventory: ArchiveInventory,
    rebuild: bool,
) -> Result<SyncSummary, AppError> {
    paths.ensure_index_dir()?;
    let mut conn = open_connection(paths, true)?;
    init_schema(&conn)?;
    if rebuild {
        clear_all(&conn)?;
    }

    let existing = load_file_state(&conn)?;
    let current_paths = inventory
        .files
        .iter()
        .map(|item| item.path.to_string())
        .collect::<BTreeSet<_>>();
    let mut removed = Vec::new();
    for path in existing.keys() {
        if !current_paths.contains(path) {
            removed.push(path.clone());
        }
    }
    let mut updated = Vec::new();
    for item in &inventory.files {
        let changed = existing
            .get(item.path.as_str())
            .map(|state| {
                state.thread_id != item.thread_id
                    || state.size != item.size
                    || state.mtime_ns != item.mtime_ns
            })
            .unwrap_or(true);
        if changed {
            updated.push(item.clone());
        }
    }

    let transaction = conn.transaction().map_err(sqlite_err)?;
    for path in &removed {
        delete_file_records(&transaction, path)?;
    }
    for item in &updated {
        delete_thread_records(&transaction, &item.thread_id)?;
        let parsed = parse_thread(item, inventory.history.get(&item.thread_id))?;
        insert_parsed_thread(&transaction, item, &parsed)?;
    }
    refresh_projects(&transaction)?;
    update_state(&transaction, &inventory)?;
    transaction.commit().map_err(sqlite_err)?;

    let project_count = conn
        .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
        .map_err(sqlite_err)?;
    let thread_count = conn
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE is_subagent = 0",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_err)?;
    let subagent_count = conn
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE is_subagent = 1",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_err)?;
    let message_count = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .map_err(sqlite_err)?;
    let event_count = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .map_err(sqlite_err)?;

    Ok(SyncSummary {
        discovered_files: inventory.files.len(),
        updated_files: updated.len(),
        removed_files: removed.len(),
        project_count,
        thread_count,
        subagent_count,
        message_count,
        event_count,
        rebuilt: rebuild,
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

fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        CREATE TABLE IF NOT EXISTS state (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS files (
            path TEXT PRIMARY KEY,
            thread_id TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS projects (
            project_slug TEXT PRIMARY KEY,
            project_cwd TEXT,
            thread_count INTEGER NOT NULL,
            last_seen_at TEXT
        );
        CREATE TABLE IF NOT EXISTS threads (
            thread_id TEXT PRIMARY KEY,
            project_slug TEXT NOT NULL,
            project_cwd TEXT,
            path TEXT NOT NULL,
            is_subagent INTEGER NOT NULL,
            parent_thread_id TEXT,
            agent_slug TEXT,
            default_scope INTEGER NOT NULL,
            title TEXT,
            started_at TEXT,
            updated_at TEXT,
            message_count INTEGER NOT NULL,
            event_count INTEGER NOT NULL,
            cli_version TEXT,
            git_branch TEXT,
            entrypoint TEXT,
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
    .map_err(sqlite_err)
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
        .prepare("SELECT path, thread_id, size, mtime_ns FROM files")
        .map_err(sqlite_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(FileState {
                path: row.get(0)?,
                thread_id: row.get(1)?,
                size: row.get(2)?,
                mtime_ns: row.get(3)?,
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
) -> Result<(), AppError> {
    let thread = &parsed.thread;
    tx.execute(
        "INSERT INTO files(path, thread_id, size, mtime_ns)
         VALUES(?1, ?2, ?3, ?4)",
        params![
            file.path.as_str(),
            file.thread_id,
            file.size,
            file.mtime_ns
        ],
    )
    .map_err(sqlite_err)?;
    tx.execute(
        "INSERT INTO threads(
            thread_id, project_slug, project_cwd, path, is_subagent, parent_thread_id,
            agent_slug, default_scope, title, started_at, updated_at,
            message_count, event_count, cli_version, git_branch, entrypoint,
            was_compacted, search_text
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            thread.thread_id,
            thread.project_slug,
            thread.project_cwd,
            thread.path.as_str(),
            bool_to_i64(thread.is_subagent),
            thread.parent_thread_id,
            thread.agent_slug,
            bool_to_i64(thread.default_scope),
            thread.title,
            thread.started_at,
            thread.updated_at,
            thread.message_count,
            thread.event_count,
            thread.cli_version,
            thread.git_branch,
            thread.entrypoint,
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
    tx.execute(
        "INSERT INTO projects(project_slug, project_cwd, thread_count, last_seen_at)
         SELECT project_slug,
                MAX(project_cwd),
                COUNT(*) FILTER (WHERE is_subagent = 0),
                MAX(updated_at)
         FROM threads
         GROUP BY project_slug",
        [],
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
        "INSERT INTO state(key, value) VALUES('history_mtime_ns', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [inventory
            .history_mtime_ns
            .map(|value| value.to_string())
            .unwrap_or_default()],
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
            "SELECT thread_id, project_slug, title, started_at, updated_at, is_subagent
             FROM threads
             WHERE thread_id = ?1",
        )
        .map_err(sqlite_err)?;
    for thread_id in thread_ids {
        let hit = stmt
            .query_row([thread_id], |row| {
                let title: Option<String> = row.get(2)?;
                Ok(ThreadSearchHit {
                    thread_id: row.get(0)?,
                    project_slug: row.get(1)?,
                    title: title.clone(),
                    started_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    is_subagent: row.get::<_, i64>(5)? != 0,
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
            "SELECT t.thread_id, t.project_slug, t.title, t.started_at, t.updated_at,
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
        project_slug: row.get(1)?,
        title: row.get(2)?,
        started_at: row.get(3)?,
        updated_at: row.get(4)?,
        is_subagent: row.get::<_, i64>(5)? != 0,
        snippet: row.get(6)?,
    })
}

fn read_thread_from_conn(
    conn: &Connection,
    thread_id: &str,
) -> Result<Option<ThreadRecord>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT thread_id, project_slug, project_cwd, title, started_at, updated_at,
                    message_count, event_count, cli_version, git_branch, entrypoint,
                    is_subagent, parent_thread_id, agent_slug, default_scope, was_compacted, path
             FROM threads WHERE thread_id = ?1",
        )
        .map_err(sqlite_err)?;
    stmt.query_row([thread_id], |row| {
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
        })
    })
    .optional()
    .map_err(sqlite_err)
}

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

    let mut by_slug = conn
        .prepare("SELECT project_slug FROM projects WHERE project_slug = ?1")
        .map_err(sqlite_err)?;
    if let Some(slug) = by_slug
        .query_row([trimmed], |row| row.get::<_, String>(0))
        .optional()
        .map_err(sqlite_err)?
    {
        return Ok(Some(slug));
    }

    let mut by_cwd = conn
        .prepare("SELECT project_slug FROM projects WHERE project_cwd = ?1")
        .map_err(sqlite_err)?;
    if let Some(slug) = by_cwd
        .query_row([trimmed], |row| row.get::<_, String>(0))
        .optional()
        .map_err(sqlite_err)?
    {
        return Ok(Some(slug));
    }

    let mut by_substring = conn
        .prepare(
            "SELECT project_slug FROM projects
             WHERE project_slug LIKE ?1 OR project_cwd LIKE ?1
             ORDER BY thread_count DESC, project_slug
             LIMIT 5",
        )
        .map_err(sqlite_err)?;
    let pattern = format!("%{trimmed}%");
    let rows = by_substring
        .query_map([pattern], |row| row.get::<_, String>(0))
        .map_err(sqlite_err)?;
    let mut matches = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_err)?;
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
