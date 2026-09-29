use assert_cmd::Command;
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const THREAD_ONE: &str = "11111111-1111-4111-8111-111111111111";
const THREAD_TWO: &str = "22222222-2222-4222-8222-222222222222";
const THREAD_THREE: &str = "33333333-3333-4333-8333-333333333333";
const COMPACT_THREAD: &str = "44444444-4444-4444-8444-444444444444";
const PROJ_TWO_THREAD: &str = "55555555-5555-4555-8555-555555555555";
const NOISY_THREAD: &str = "66666666-6666-4666-8666-666666666666";
const SUBAGENT_THREAD_ID: &str = "11111111-1111-4111-8111-111111111111:agent:aaaa1111bbbb2222";
const WORKFLOW_SUBAGENT_ID: &str = "11111111-1111-4111-8111-111111111111:agent:cccc3333dddd4444";
const AI_TITLE_THREAD: &str = "abababab-abab-4bab-8bab-abababababab";
const MIXED_THREAD: &str = "cdcdcdcd-cdcd-4dcd-8dcd-cdcdcdcdcdcd";
const STUB_THREAD: &str = "efefefef-efef-4fef-8fef-efefefefefef";
const COWORK_SPACE_THREAD: &str = "c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1";
const COWORK_SUBAGENT_ID: &str = "c1c1c1c1-c1c1-4c1c-8c1c-c1c1c1c1c1c1:agent:a1b2c3d4e5f6a7b8";
const COWORK_LOOSE_THREAD: &str = "e1e1e1e1-e1e1-4e1e-8e1e-e1e1e1e1e1e1";

const PROJECT_ONE_SLUG: &str = "-fixture-project-one";
const PROJECT_TWO_SLUG: &str = "-fixture-project-two";
const COWORK_SPACE_SLUG: &str = "cowork:space-fixture";
const COWORK_SLUG: &str = "cowork";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-home")
}

fn desktop_fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-desktop")
}

/// Copies both fixture trees: the Claude Code home at the temp root and the
/// desktop app data (Cowork + Code tab metadata) under `desktop/`.
fn copied_fixture_home() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    copy_dir_all(&fixture_root(), temp.path());
    copy_dir_all(&desktop_fixture_root(), &temp.path().join("desktop"));
    temp
}

fn copy_dir_all(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create fixture dir");
    for entry in fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let file_type = entry.file_type().expect("fixture type");
        let dest = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).expect("copy fixture file");
        }
    }
}

fn bin(temp: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("claude-threads").expect("cargo bin");
    cmd.env("CLAUDE_HOME", temp.path());
    cmd.env("CLAUDE_DESKTOP_HOME", temp.path().join("desktop"));
    cmd
}

fn run_json(temp: &TempDir, args: &[&str]) -> (i32, Value, String) {
    let output = bin(temp).args(args).output().expect("command output");
    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    let value = serde_json::from_str::<Value>(&stdout).expect("json stdout");
    (status, value, stderr)
}

#[test]
fn sync_indexes_fixture_archives() {
    let temp = copied_fixture_home();
    let (status, json, _stderr) = run_json(&temp, &["--json", "sync"]);
    assert_eq!(status, 0);
    assert_eq!(json["ok"], true);
    // 11 Claude Code transcripts (workflow journal excluded) + 3 Cowork ones;
    // Cowork audit logs, outputs/, and plugin payloads are never scanned.
    assert_eq!(json["data"]["discovered_files"], 14);
    assert_eq!(json["data"]["project_count"], 4);
    assert_eq!(json["data"]["thread_count"], 10);
    assert_eq!(json["data"]["subagent_count"], 3);
    assert_eq!(json["data"]["cowork_thread_count"], 2);
    assert_eq!(json["data"]["message_count"], 31);
    assert_eq!(json["data"]["event_count"], 46);
    assert_eq!(json["data"]["skipped_empty_files"], 1);
}

#[test]
fn projects_list_returns_known_projects() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(&temp, &["--json", "projects", "list"]);
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 4);
    let slugs: Vec<&str> = items
        .iter()
        .map(|item| item["project_slug"].as_str().unwrap())
        .collect();
    assert!(slugs.contains(&PROJECT_ONE_SLUG));
    assert!(slugs.contains(&PROJECT_TWO_SLUG));
    let space = items
        .iter()
        .find(|item| item["project_slug"] == COWORK_SPACE_SLUG)
        .expect("cowork space project");
    assert_eq!(space["source"], "cowork");
    assert_eq!(space["project_name"], "Fixture Space");
    assert_eq!(space["project_cwd"], "/workspace/cowork-space");
    let loose = items
        .iter()
        .find(|item| item["project_slug"] == COWORK_SLUG)
        .expect("catch-all cowork project");
    assert_eq!(loose["project_name"], "Cowork");
    assert!(loose["project_cwd"].is_null());
    let one = items
        .iter()
        .find(|item| item["project_slug"] == PROJECT_ONE_SLUG)
        .expect("project one");
    assert_eq!(one["source"], "claude_code");
}

#[test]
fn threads_search_lazy_sync_filters_out_subagents() {
    let temp = copied_fixture_home();
    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "search",
            "build a CLI",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["ok"], true);
    assert_eq!(json["meta"]["auto_sync_performed"], true);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["thread_id"], THREAD_ONE);
    assert_eq!(items[0]["project_slug"], PROJECT_ONE_SLUG);
    assert_eq!(items[0]["is_subagent"], false);
}

#[test]
fn threads_search_include_subagents_surfaces_sidechain() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "search",
            "archive format",
            "--include-subagents",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["thread_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&THREAD_ONE));
    assert!(ids.contains(&SUBAGENT_THREAD_ID));
}

#[test]
fn threads_resolve_reports_ambiguity() {
    let temp = copied_fixture_home();
    let (status, json, _stderr) = run_json(&temp, &["--json", "threads", "resolve", "tweet idea"]);
    assert_eq!(status, 6);
    assert_eq!(json["error"]["code"], "ambiguous");
    let candidates = json["error"]["details"]["candidates"]
        .as_array()
        .expect("candidate array");
    assert_eq!(candidates.len(), 2);
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate["thread_id"].is_string())
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate["title"] == "Tweet idea")
    );
}

#[test]
fn threads_resolve_returns_thread_id_directly() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) =
        run_json(&temp, &["--json", "threads", "resolve", PROJ_TWO_THREAD]);
    assert_eq!(status, 0);
    assert_eq!(json["data"]["thread"]["thread_id"], PROJ_TWO_THREAD);
    assert_eq!(json["data"]["thread"]["project_slug"], PROJECT_TWO_SLUG);
}

#[test]
fn threads_read_returns_exact_thread() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(&temp, &["--json", "threads", "read", THREAD_ONE]);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["thread_id"], THREAD_ONE);
    assert_eq!(
        thread["title"],
        "build a CLI for searching past Claude threads"
    );
    assert_eq!(thread["project_slug"], PROJECT_ONE_SLUG);
    assert_eq!(thread["project_cwd"], "/workspace/project-one");
    assert_eq!(thread["message_count"], 3);
    assert_eq!(thread["was_compacted"], false);
    assert_eq!(thread["is_subagent"], false);
    assert_eq!(thread["git_branch"], "main");
    assert_eq!(thread["entrypoint"], "cli");
    assert_eq!(thread["cli_version"], "2.1.101");
}

#[test]
fn compact_thread_flagged_and_titled_from_real_message() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(&temp, &["--json", "threads", "read", COMPACT_THREAD]);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["was_compacted"], true);
    assert_eq!(thread["title"], "continue building the CLI now");
    assert_eq!(thread["message_count"], 3);
}

#[test]
fn noisy_thread_title_falls_back_to_history_enrichment() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(&temp, &["--json", "threads", "read", NOISY_THREAD]);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["title"], "Find the noisy thread fixture entry");
    assert_eq!(thread["message_count"], 0);
}

#[test]
fn messages_search_and_read_return_normalized_messages() {
    let temp = copied_fixture_home();
    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "search",
            "archive format",
            "--limit",
            "5",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    let message_id = items[0]["message_id"].as_str().expect("message id");
    assert!(message_id.starts_with(THREAD_ONE));
    assert_eq!(items[0]["role"], "assistant");

    let (read_status, read_json, _stderr) =
        run_json(&temp, &["--json", "messages", "read", message_id]);
    assert_eq!(read_status, 0);
    let message = &read_json["data"]["message"];
    assert_eq!(message["role"], "assistant");
    assert!(
        message["text"]
            .as_str()
            .expect("message text")
            .contains("inspect the archive format")
    );
    assert_eq!(message["project_slug"], PROJECT_ONE_SLUG);
}

#[test]
fn messages_search_role_filter_narrows_results() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json", "messages", "search", "Tweet", "--role", "user", "--limit", "10",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert!(items.iter().all(|item| item["role"] == "user"));
    assert!(items.len() >= 2);
}

#[test]
fn messages_search_invalid_role_returns_usage_error() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &["--json", "messages", "search", "Tweet", "--role", "robot"],
    );
    assert_eq!(status, 2);
    assert_eq!(json["error"]["code"], "usage_error");
}

#[test]
fn messages_search_project_filter_excludes_other_projects() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "search",
            "audit",
            "--project",
            PROJECT_ONE_SLUG,
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert!(items.is_empty());

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "search",
            "audit",
            "--project",
            PROJECT_TWO_SLUG,
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert!(
        items
            .iter()
            .any(|item| item["thread_id"] == PROJ_TWO_THREAD)
    );
}

#[test]
fn events_read_returns_payloads_and_limit() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &["--json", "events", "read", THREAD_ONE, "--limit", "3"],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["payload"]["sessionId"], THREAD_ONE);
    assert_eq!(items[0]["record_type"], "user");
}

#[test]
fn index_stats_and_debug_paths_are_available() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (stats_status, stats_json, _stderr) = run_json(&temp, &["--json", "index", "stats"]);
    assert_eq!(stats_status, 0);
    assert_eq!(stats_json["meta"]["auto_sync_performed"], false);
    assert_eq!(stats_json["data"]["thread_count"], 10);
    assert_eq!(stats_json["data"]["subagent_count"], 3);
    assert_eq!(stats_json["data"]["cowork_thread_count"], 2);
    assert_eq!(stats_json["data"]["skipped_empty_files"], 1);
    assert_eq!(stats_json["data"]["project_count"], 4);

    let (debug_status, debug_json, _stderr) = run_json(&temp, &["--json", "debug", "paths"]);
    assert_eq!(debug_status, 0);
    assert_eq!(debug_json["data"]["projects_root_exists"], true);
    assert_eq!(debug_json["data"]["index_exists"], true);
    assert_eq!(debug_json["data"]["history_exists"], true);
    assert_eq!(debug_json["data"]["cowork_root_exists"], true);
    assert_eq!(debug_json["data"]["desktop_sessions_root_exists"], true);
}

#[test]
fn malformed_jsonl_lines_are_skipped_with_warning() {
    let temp = copied_fixture_home();
    let broken_path = temp
        .path()
        .join("projects/-fixture-project-two/77777777-7777-4777-8777-777777777777.jsonl");
    fs::write(
        &broken_path,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"valid first line\"},\"sessionId\":\"77777777-7777-4777-8777-777777777777\",\"timestamp\":\"2026-04-11T16:00:00.000Z\"}\nnot-json\n",
    )
    .expect("write broken fixture");

    let (status, json, stderr) = run_json(&temp, &["--json", "sync", "--rebuild"]);
    assert_eq!(status, 0);
    assert_eq!(json["ok"], true);
    assert!(stderr.contains("skipping malformed JSONL"));
    assert_eq!(json["data"]["discovered_files"], 15);

    let (read_status, read_json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "read",
            "77777777-7777-4777-8777-777777777777",
        ],
    );
    assert_eq!(read_status, 0);
    assert_eq!(read_json["data"]["thread"]["message_count"], 1);
    assert_eq!(read_json["data"]["thread"]["event_count"], 1);
}

#[test]
fn malformed_history_is_ignored_as_enrichment_only() {
    let temp = copied_fixture_home();
    let history_path = temp.path().join("history.jsonl");
    let original = fs::read_to_string(&history_path).expect("read history");
    fs::write(&history_path, format!("{original}\nnot-json\n")).expect("write malformed history");

    let (status, json, stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "search",
            "build a CLI",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["ok"], true);
    assert!(stderr.contains("ignoring malformed history.jsonl line"));
}

#[test]
fn exact_reads_return_not_found_with_stable_exit_code() {
    let temp = copied_fixture_home();
    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "read",
            "99999999-9999-4999-8999-999999999999",
        ],
    );
    assert_eq!(status, 5);
    assert_eq!(json["error"]["code"], "not_found");
}

#[test]
fn search_results_do_not_include_subagent_thread_by_default() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &["--json", "threads", "search", "review", "--limit", "10"],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert!(
        items
            .iter()
            .all(|item| item["thread_id"] != SUBAGENT_THREAD_ID)
    );
}

#[test]
fn subagent_thread_is_readable_by_exact_id() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) =
        run_json(&temp, &["--json", "threads", "read", SUBAGENT_THREAD_ID]);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["thread_id"], SUBAGENT_THREAD_ID);
    assert_eq!(thread["is_subagent"], true);
    assert_eq!(thread["parent_thread_id"], THREAD_ONE);
    assert_eq!(thread["agent_slug"], "aside_question");
}

#[test]
fn empty_search_query_returns_usage_error() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &["--json", "threads", "search", "   ", "--limit", "10"],
    );
    assert_eq!(status, 2);
    assert_eq!(json["error"]["code"], "usage_error");

    let (status, json, _stderr) = run_json(
        &temp,
        &["--json", "messages", "search", "", "--limit", "10"],
    );
    assert_eq!(status, 2);
    assert_eq!(json["error"]["code"], "usage_error");
}

#[test]
fn project_filter_accepts_full_cwd() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "search",
            "audit",
            "--project",
            "/workspace/project-two",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert!(
        items
            .iter()
            .any(|item| item["thread_id"] == PROJ_TWO_THREAD)
    );
}

#[test]
fn events_read_on_unknown_thread_returns_not_found() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "events",
            "read",
            "99999999-9999-4999-8999-999999999999",
            "--limit",
            "5",
        ],
    );
    assert_eq!(status, 5);
    assert_eq!(json["error"]["code"], "not_found");
}

#[test]
fn messages_search_include_subagents_surfaces_sidechain_messages() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (default_status, default_json, _stderr) = run_json(
        &temp,
        &["--json", "messages", "search", "review", "--limit", "10"],
    );
    assert_eq!(default_status, 0);
    let default_items = default_json["data"]["items"]
        .as_array()
        .expect("items array");
    assert!(
        default_items
            .iter()
            .all(|item| item["thread_id"] != SUBAGENT_THREAD_ID)
    );

    let (with_sub_status, with_sub_json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "search",
            "review",
            "--include-subagents",
            "--limit",
            "10",
        ],
    );
    assert_eq!(with_sub_status, 0);
    let with_sub_items = with_sub_json["data"]["items"]
        .as_array()
        .expect("items array");
    assert!(
        with_sub_items
            .iter()
            .any(|item| item["thread_id"] == SUBAGENT_THREAD_ID)
    );
}

#[test]
fn explicit_sync_then_search_is_not_stale() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(
        &temp,
        &["--json", "threads", "search", "tweet idea", "--limit", "10"],
    );
    assert_eq!(status, 0);
    assert_eq!(json["meta"]["auto_sync_performed"], false);
    let items = json["data"]["items"].as_array().expect("items array");
    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["thread_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&THREAD_TWO));
    assert!(ids.contains(&THREAD_THREE));
}

#[test]
fn search_uses_existing_index_when_auto_sync_writer_is_locked() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);

    let history_path = temp.path().join("history.jsonl");
    let history_contents = fs::read_to_string(&history_path).expect("read history");
    fs::write(&history_path, history_contents).expect("touch history");

    let index_path = temp.path().join("claude-threads/index.sqlite");
    let conn = Connection::open(index_path).expect("open sqlite");
    conn.execute_batch("PRAGMA journal_mode = WAL; BEGIN IMMEDIATE;")
        .expect("lock sqlite writer");

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "search",
            "build a CLI",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["ok"], true);
    assert_eq!(json["meta"]["auto_sync_performed"], false);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["thread_id"], THREAD_ONE);

    conn.execute_batch("ROLLBACK;").expect("unlock sqlite");
}

// -------------------------------------------------------------------------
// Chronological listing (threads list / messages list) — fixtures + tests
// -------------------------------------------------------------------------

const THREAD_CHRONO_EARLY: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const THREAD_CHRONO_MID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const THREAD_CHRONO_LATE: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const THREAD_CHRONO_UNTIMESTAMPED: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
const PROJECT_CHRONO_SLUG: &str = "-fixture-project-chrono";
const PROJECT_CHRONO_CWD: &str = "/workspace/project-chrono";

fn write_session_raw(temp: &TempDir, relative_path: &str, contents: &str) {
    let full_path = temp.path().join(relative_path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).expect("create session parent dir");
    }
    fs::write(&full_path, contents).expect("write session file");
}

fn add_chrono_project_threads(temp: &TempDir) {
    write_session_raw(
        temp,
        "projects/-fixture-project-chrono/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl",
        concat!(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-chrono-early-1","type":"user","message":{"role":"user","content":"first chrono project question"},"uuid":"chrono-early-u-1","timestamp":"2026-04-11T08:00:00.000Z","userType":"external","entrypoint":"cli","cwd":"/workspace/project-chrono","sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","version":"2.1.101","gitBranch":"main"}"#,
            "\n",
            r#"{"parentUuid":"chrono-early-u-1","isSidechain":false,"message":{"model":"claude-opus-4-6","id":"msg_chrono_early_a1","type":"message","role":"assistant","content":[{"type":"text","text":"earliest chrono reply"}],"stop_reason":"end_turn"},"requestId":"req_chrono_early_a1","type":"assistant","uuid":"chrono-early-a-1","timestamp":"2026-04-11T08:00:01.000Z","userType":"external","entrypoint":"cli","cwd":"/workspace/project-chrono","sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","version":"2.1.101","gitBranch":"main"}"#,
            "\n",
        ),
    );
    write_session_raw(
        temp,
        "projects/-fixture-project-chrono/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb.jsonl",
        concat!(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-chrono-mid-1","type":"user","message":{"role":"user","content":"middle chrono project question"},"uuid":"chrono-mid-u-1","timestamp":"2026-04-11T10:00:00.000Z","userType":"external","entrypoint":"cli","cwd":"/workspace/project-chrono","sessionId":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","version":"2.1.101","gitBranch":"main"}"#,
            "\n",
            r#"{"parentUuid":"chrono-mid-u-1","isSidechain":false,"message":{"model":"claude-opus-4-6","id":"msg_chrono_mid_a1","type":"message","role":"assistant","content":[{"type":"text","text":"middle chrono reply"}],"stop_reason":"end_turn"},"requestId":"req_chrono_mid_a1","type":"assistant","uuid":"chrono-mid-a-1","timestamp":"2026-04-11T10:00:01.000Z","userType":"external","entrypoint":"cli","cwd":"/workspace/project-chrono","sessionId":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","version":"2.1.101","gitBranch":"main"}"#,
            "\n",
        ),
    );
    write_session_raw(
        temp,
        "projects/-fixture-project-chrono/cccccccc-cccc-4ccc-8ccc-cccccccccccc.jsonl",
        concat!(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-chrono-late-1","type":"user","message":{"role":"user","content":"last chrono project question"},"uuid":"chrono-late-u-1","timestamp":"2026-04-11T12:30:00.000Z","userType":"external","entrypoint":"cli","cwd":"/workspace/project-chrono","sessionId":"cccccccc-cccc-4ccc-8ccc-cccccccccccc","version":"2.1.101","gitBranch":"main"}"#,
            "\n",
            r#"{"parentUuid":"chrono-late-u-1","isSidechain":false,"message":{"model":"claude-opus-4-6","id":"msg_chrono_late_a1","type":"message","role":"assistant","content":[{"type":"text","text":"latest chrono reply"}],"stop_reason":"end_turn"},"requestId":"req_chrono_late_a1","type":"assistant","uuid":"chrono-late-a-1","timestamp":"2026-04-11T12:30:01.000Z","userType":"external","entrypoint":"cli","cwd":"/workspace/project-chrono","sessionId":"cccccccc-cccc-4ccc-8ccc-cccccccccccc","version":"2.1.101","gitBranch":"main"}"#,
            "\n",
        ),
    );
}

fn add_chrono_untimestamped_thread(temp: &TempDir) {
    write_session_raw(
        temp,
        "projects/-fixture-project-chrono/dddddddd-dddd-4ddd-8ddd-dddddddddddd.jsonl",
        concat!(
            r#"{"parentUuid":null,"isSidechain":false,"promptId":"prompt-chrono-unts-1","type":"user","message":{"role":"user","content":"untimestamped chrono project question"},"uuid":"chrono-unts-u-1","userType":"external","entrypoint":"cli","cwd":"/workspace/project-chrono","sessionId":"dddddddd-dddd-4ddd-8ddd-dddddddddddd","version":"2.1.101","gitBranch":"main"}"#,
            "\n",
            r#"{"parentUuid":"chrono-unts-u-1","isSidechain":false,"message":{"model":"claude-opus-4-6","id":"msg_chrono_unts_a1","type":"message","role":"assistant","content":[{"type":"text","text":"untimestamped chrono reply"}],"stop_reason":"end_turn"},"requestId":"req_chrono_unts_a1","type":"assistant","uuid":"chrono-unts-a-1","userType":"external","entrypoint":"cli","cwd":"/workspace/project-chrono","sessionId":"dddddddd-dddd-4ddd-8ddd-dddddddddddd","version":"2.1.101","gitBranch":"main"}"#,
            "\n",
        ),
    );
}

#[test]
fn threads_list_orders_chronologically_with_nulls_last() {
    let temp = copied_fixture_home();
    add_chrono_project_threads(&temp);
    add_chrono_untimestamped_thread(&temp);
    let _ = run_json(&temp, &["--json", "sync"]);

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "list",
            "--project",
            PROJECT_CHRONO_SLUG,
            "--order",
            "asc",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["data"]["order"], "asc");
    assert_eq!(json["data"]["project"], PROJECT_CHRONO_SLUG);
    assert_eq!(json["data"]["include_subagents"], false);
    let items = json["data"]["items"].as_array().expect("items array");
    let thread_ids = items
        .iter()
        .map(|item| item["thread_id"].as_str().expect("thread id"))
        .collect::<Vec<_>>();
    assert_eq!(
        thread_ids,
        vec![
            THREAD_CHRONO_EARLY,
            THREAD_CHRONO_MID,
            THREAD_CHRONO_LATE,
            THREAD_CHRONO_UNTIMESTAMPED,
        ]
    );
    assert!(items.iter().all(|item| item["default_scope"] == true));
    assert!(items.iter().all(|item| item["is_subagent"] == false));

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "list",
            "--project",
            PROJECT_CHRONO_SLUG,
            "--order",
            "desc",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["data"]["order"], "desc");
    let items = json["data"]["items"].as_array().expect("items array");
    let thread_ids = items
        .iter()
        .map(|item| item["thread_id"].as_str().expect("thread id"))
        .collect::<Vec<_>>();
    assert_eq!(
        thread_ids,
        vec![
            THREAD_CHRONO_LATE,
            THREAD_CHRONO_MID,
            THREAD_CHRONO_EARLY,
            THREAD_CHRONO_UNTIMESTAMPED,
        ]
    );
}

#[test]
fn messages_list_supports_first_and_last_user_queries() {
    let temp = copied_fixture_home();
    add_chrono_project_threads(&temp);
    add_chrono_untimestamped_thread(&temp);
    let _ = run_json(&temp, &["--json", "sync"]);

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "list",
            "--project",
            PROJECT_CHRONO_CWD,
            "--role",
            "user",
            "--order",
            "asc",
            "--limit",
            "1",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["data"]["order"], "asc");
    assert_eq!(json["data"]["role"], "user");
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["thread_id"], THREAD_CHRONO_EARLY);
    assert_eq!(items[0]["text"], "first chrono project question");
    assert_eq!(items[0]["role"], "user");

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "list",
            "--project",
            PROJECT_CHRONO_CWD,
            "--role",
            "user",
            "--order",
            "desc",
            "--limit",
            "1",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["data"]["order"], "desc");
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["thread_id"], THREAD_CHRONO_LATE);
    assert_eq!(items[0]["text"], "last chrono project question");

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "list",
            "--project",
            PROJECT_CHRONO_SLUG,
            "--role",
            "user",
            "--order",
            "asc",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    let thread_ids = items
        .iter()
        .map(|item| item["thread_id"].as_str().expect("thread id"))
        .collect::<Vec<_>>();
    assert_eq!(
        thread_ids,
        vec![
            THREAD_CHRONO_EARLY,
            THREAD_CHRONO_MID,
            THREAD_CHRONO_LATE,
            THREAD_CHRONO_UNTIMESTAMPED,
        ]
    );
    assert!(items.iter().all(|item| item["role"] == "user"));
}

#[test]
fn messages_list_excludes_subagent_messages_by_default() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "list",
            "--project",
            PROJECT_ONE_SLUG,
            "--order",
            "asc",
            "--limit",
            "50",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert!(!items.is_empty());
    assert!(
        items
            .iter()
            .all(|item| item["project_slug"] == PROJECT_ONE_SLUG)
    );
    assert!(
        items
            .iter()
            .all(|item| item["thread_id"] != SUBAGENT_THREAD_ID)
    );
}

#[test]
fn threads_list_include_subagents_surfaces_sidechain() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);

    let (default_status, default_json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "list",
            "--project",
            PROJECT_ONE_SLUG,
            "--limit",
            "20",
        ],
    );
    assert_eq!(default_status, 0);
    let default_items = default_json["data"]["items"]
        .as_array()
        .expect("items array");
    assert!(
        default_items
            .iter()
            .all(|item| item["thread_id"] != SUBAGENT_THREAD_ID)
    );

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "list",
            "--project",
            PROJECT_ONE_SLUG,
            "--include-subagents",
            "--limit",
            "20",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["data"]["include_subagents"], true);
    let items = json["data"]["items"].as_array().expect("items array");
    let subagent_entry = items
        .iter()
        .find(|item| item["thread_id"] == SUBAGENT_THREAD_ID)
        .expect("subagent thread surfaced with --include-subagents");
    assert_eq!(subagent_entry["is_subagent"], true);
    assert_eq!(subagent_entry["default_scope"], false);
}

// -------------------------------------------------------------------------
// Current Claude Code schema: titles, message authorship, stubs, workflows
// -------------------------------------------------------------------------

fn read_thread(temp: &TempDir, thread_id: &str) -> (i32, Value) {
    let (status, json, _stderr) = run_json(temp, &["--json", "threads", "read", thread_id]);
    (status, json)
}

#[test]
fn ai_and_custom_titles_take_precedence_over_first_prompt() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);

    let (status, json) = read_thread(&temp, AI_TITLE_THREAD);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["title"], "Design the FTS ranking strategy");
    assert_eq!(thread["model"], "claude-opus-5");
    assert_eq!(thread["entrypoint"], "claude-desktop");
    assert_eq!(thread["source"], "claude_code");
    // The synthetic API error reply is not indexed as assistant text.
    assert_eq!(thread["message_count"], 2);

    let (status, json) = read_thread(&temp, MIXED_THREAD);
    assert_eq!(status, 0);
    assert_eq!(
        json["data"]["thread"]["title"],
        "Migration checks (renamed)"
    );

    // Thread search still matches the first prompt and the superseded AI title.
    for query in ["rank sqlite fts", "Auto title for mixed thread"] {
        let (status, json, _stderr) = run_json(
            &temp,
            &["--json", "threads", "search", query, "--limit", "5"],
        );
        assert_eq!(status, 0);
        let items = json["data"]["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1, "query {query}");
    }
}

#[test]
fn harness_authored_messages_use_system_role() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "list",
            "--project",
            PROJECT_ONE_SLUG,
            "--order",
            "asc",
            "--limit",
            "100",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    let mixed: Vec<(&str, &str, &str)> = items
        .iter()
        .filter(|item| item["thread_id"] == MIXED_THREAD)
        .map(|item| {
            (
                item["role"].as_str().unwrap(),
                item["kind"].as_str().unwrap(),
                item["text"].as_str().unwrap(),
            )
        })
        .collect();
    let shape: Vec<(&str, &str)> = mixed.iter().map(|(role, kind, _)| (*role, *kind)).collect();
    assert_eq!(
        shape,
        vec![
            ("user", "user_message"),
            ("assistant", "assistant_text"),
            ("user", "user_message"),
            ("system", "task_notification"),
            ("system", "task_notification"),
            ("system", "peer_message"),
            ("system", "system_event"),
            ("system", "away_summary"),
        ]
    );
    // The prompt typed while the agent was busy arrives as a queued_command.
    assert_eq!(mixed[2].2, "also verify the rollback path");
    assert!(
        mixed
            .iter()
            .all(|(_, _, text)| !text.contains("hidden plumbing"))
    );
    assert!(
        mixed
            .iter()
            .all(|(_, _, text)| !text.contains("Base directory"))
    );

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "search",
            "release window",
            "--role",
            "system",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "peer_message");

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "search",
            "overloaded_error",
            "--limit",
            "5",
        ],
    );
    assert_eq!(status, 0);
    assert!(json["data"]["items"].as_array().expect("items").is_empty());
}

#[test]
fn stub_transcripts_without_conversation_are_skipped() {
    let temp = copied_fixture_home();
    let (status, json, _stderr) = run_json(&temp, &["--json", "sync"]);
    assert_eq!(status, 0);
    assert_eq!(json["data"]["skipped_empty_files"], 1);

    let (status, json) = read_thread(&temp, STUB_THREAD);
    assert_eq!(status, 5);
    assert_eq!(json["error"]["code"], "not_found");

    // Tracking the stub keeps the index fresh instead of re-syncing forever.
    let (status, json, _stderr) = run_json(&temp, &["--json", "index", "stats"]);
    assert_eq!(status, 0);
    assert_eq!(json["meta"]["auto_sync_performed"], false);
}

#[test]
fn workflow_subagents_are_indexed_with_agent_metadata() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json) = read_thread(&temp, WORKFLOW_SUBAGENT_ID);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["is_subagent"], true);
    assert_eq!(thread["default_scope"], false);
    assert_eq!(thread["parent_thread_id"], THREAD_ONE);
    assert_eq!(thread["workflow_id"], "wf_fixture-001");
    assert_eq!(thread["agent_type"], "workflow-subagent");
    assert_eq!(thread["title"], "Verify workflow fixture wiring");
    assert_eq!(thread["message_count"], 2);
}

#[test]
fn desktop_code_session_metadata_enriches_threads() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json) = read_thread(&temp, PROJ_TWO_THREAD);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["title"], "Desktop renamed ADR session");
    assert_eq!(thread["is_archived"], true);
    assert_eq!(
        thread["app_session_id"],
        "local_d5d5d5d5-d5d5-4d5d-8d5d-d5d5d5d5d5d5"
    );
    // Archived threads stay in the default scope.
    assert_eq!(thread["default_scope"], true);
}

// -------------------------------------------------------------------------
// Cowork sessions (Claude desktop local-agent-mode-sessions)
// -------------------------------------------------------------------------

#[test]
fn cowork_sessions_are_indexed_across_layouts() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json", "threads", "list", "--source", "cowork", "--limit", "10",
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(json["data"]["source"], "cowork");
    let items = json["data"]["items"].as_array().expect("items array");
    let ids: Vec<&str> = items
        .iter()
        .map(|item| item["thread_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![COWORK_LOOSE_THREAD, COWORK_SPACE_THREAD]);
    assert!(items.iter().all(|item| item["source"] == "cowork"));

    let (status, json) = read_thread(&temp, COWORK_SPACE_THREAD);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["title"], "Quarterly board deck");
    assert_eq!(thread["project_slug"], COWORK_SPACE_SLUG);
    assert_eq!(thread["project_name"], "Fixture Space");
    assert_eq!(thread["project_cwd"], "/workspace/cowork-space");
    assert_eq!(thread["entrypoint"], "local-agent");
    assert_eq!(thread["model"], "claude-opus-5-5");
    assert_eq!(
        thread["app_session_id"],
        "local_c0c0c0c0-c0c0-4c0c-8c0c-c0c0c0c0c0c0"
    );
    assert_eq!(thread["is_archived"], false);
    assert_eq!(
        thread["selected_folders"],
        serde_json::json!(["/workspace/cowork-space", "/workspace/cowork-extra"])
    );

    let (status, json) = read_thread(&temp, COWORK_LOOSE_THREAD);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["title"], "Customize Claude to your role");
    assert_eq!(thread["project_slug"], COWORK_SLUG);
    assert_eq!(thread["project_cwd"], "/workspace/cowork-loose");
    assert_eq!(thread["is_archived"], true);

    let (status, json) = read_thread(&temp, COWORK_SUBAGENT_ID);
    assert_eq!(status, 0);
    let thread = &json["data"]["thread"];
    assert_eq!(thread["is_subagent"], true);
    assert_eq!(thread["project_slug"], COWORK_SPACE_SLUG);
    assert_eq!(thread["agent_type"], "general-purpose");
    assert_eq!(thread["title"], "Summarize finance workbook");
    assert!(thread["app_session_id"].is_null());
}

#[test]
fn cowork_threads_filter_by_source_and_space_name() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "messages",
            "search",
            "board deck",
            "--source",
            "cowork",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert!(!items.is_empty());
    assert!(items.iter().all(|item| item["source"] == "cowork"));
    assert!(
        items
            .iter()
            .all(|item| item["thread_id"] == COWORK_SPACE_THREAD)
    );

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "search",
            "board deck",
            "--source",
            "claude-code",
        ],
    );
    assert_eq!(status, 0);
    assert!(json["data"]["items"].as_array().expect("items").is_empty());

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "list",
            "--project",
            "fixture space",
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["thread_id"], COWORK_SPACE_THREAD);

    let (status, json, _stderr) = run_json(
        &temp,
        &[
            "--json",
            "threads",
            "list",
            "--project",
            COWORK_SLUG,
            "--limit",
            "10",
        ],
    );
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["thread_id"], COWORK_LOOSE_THREAD);

    let (status, json, _stderr) = run_json(
        &temp,
        &["--json", "threads", "resolve", "Quarterly board deck"],
    );
    assert_eq!(status, 0);
    assert_eq!(json["data"]["thread"]["thread_id"], COWORK_SPACE_THREAD);
}

#[test]
fn cowork_only_machine_indexes_without_claude_code_projects() {
    let temp = copied_fixture_home();
    fs::remove_dir_all(temp.path().join("projects")).expect("remove projects");
    let (status, json, _stderr) = run_json(&temp, &["--json", "sync"]);
    assert_eq!(status, 0);
    assert_eq!(json["data"]["thread_count"], 2);
    assert_eq!(json["data"]["cowork_thread_count"], 2);
}

#[test]
fn missing_archives_return_archive_not_found() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (status, json, _stderr) = run_json(&temp, &["--json", "threads", "list"]);
    assert_eq!(status, 3);
    assert_eq!(json["error"]["code"], "archive_not_found");
}

// -------------------------------------------------------------------------
// Freshness: metadata-only changes and schema upgrades
// -------------------------------------------------------------------------

#[test]
fn metadata_only_changes_retitle_unchanged_transcripts() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);

    write_session_raw(
        &temp,
        &format!("projects/{PROJECT_ONE_SLUG}/{AI_TITLE_THREAD}/custom-title.json"),
        r#"{"customTitle":"Renamed from the sidecar"}"#,
    );
    let history_path = temp.path().join("history.jsonl");
    let history = fs::read_to_string(&history_path).expect("read history");
    fs::write(
        &history_path,
        format!(
            "{history}{}\n",
            r#"{"display":"Updated noisy history entry","pastedContents":{},"timestamp":1775000099000,"project":"/workspace/project-two","sessionId":"66666666-6666-4666-8666-666666666666"}"#
        ),
    )
    .expect("append history");

    let (status, json, _stderr) = run_json(&temp, &["--json", "threads", "read", AI_TITLE_THREAD]);
    assert_eq!(status, 0);
    assert_eq!(json["meta"]["auto_sync_performed"], true);
    assert_eq!(json["data"]["thread"]["title"], "Renamed from the sidecar");

    let (status, json) = read_thread(&temp, NOISY_THREAD);
    assert_eq!(status, 0);
    assert_eq!(json["meta"]["auto_sync_performed"], false);
    assert_eq!(
        json["data"]["thread"]["title"],
        "Updated noisy history entry"
    );

    let (status, json, _stderr) = run_json(&temp, &["--json", "sync"]);
    assert_eq!(status, 0);
    assert_eq!(json["data"]["updated_files"], 0);
}

#[test]
fn cowork_rename_and_archive_resync_only_that_session() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let meta_path = temp.path().join(
        "desktop/local-agent-mode-sessions/acct-fixture/org-fixture/local_e0e0e0e0-e0e0-4e0e-8e0e-e0e0e0e0e0e0.json",
    );
    let original = fs::read_to_string(&meta_path).expect("read cowork meta");
    fs::write(
        &meta_path,
        original
            .replace("Customize Claude to your role", "Role setup (renamed)")
            .replace("\"isArchived\":true", "\"isArchived\":false"),
    )
    .expect("write cowork meta");

    let (status, json, _stderr) = run_json(&temp, &["--json", "sync"]);
    assert_eq!(status, 0);
    assert_eq!(json["data"]["updated_files"], 1);
    let (status, json) = read_thread(&temp, COWORK_LOOSE_THREAD);
    assert_eq!(status, 0);
    assert_eq!(json["data"]["thread"]["title"], "Role setup (renamed)");
    assert_eq!(json["data"]["thread"]["is_archived"], false);
}

#[test]
fn outdated_index_schema_is_rebuilt_automatically() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let index_path = temp.path().join("claude-threads/index.sqlite");
    {
        let conn = Connection::open(&index_path).expect("open sqlite");
        conn.execute_batch("ALTER TABLE threads DROP COLUMN source; PRAGMA user_version = 1;")
            .expect("simulate old schema");
    }

    let (status, json) = read_thread(&temp, COWORK_SPACE_THREAD);
    assert_eq!(status, 0);
    assert_eq!(json["meta"]["auto_sync_performed"], true);
    assert_eq!(json["data"]["thread"]["source"], "cowork");

    let conn = Connection::open(&index_path).expect("open sqlite");
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version");
    assert_eq!(version, 2);
}
