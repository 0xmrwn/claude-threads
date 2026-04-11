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

const PROJECT_ONE_SLUG: &str = "-fixture-project-one";
const PROJECT_TWO_SLUG: &str = "-fixture-project-two";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-home")
}

fn copied_fixture_home() -> TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    copy_dir_all(&fixture_root(), temp.path());
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
    assert_eq!(json["data"]["discovered_files"], 7);
    assert_eq!(json["data"]["project_count"], 2);
    assert_eq!(json["data"]["thread_count"], 6);
    assert_eq!(json["data"]["subagent_count"], 1);
    assert_eq!(json["data"]["message_count"], 14);
    assert_eq!(json["data"]["event_count"], 19);
}

#[test]
fn projects_list_returns_known_projects() {
    let temp = copied_fixture_home();
    let _ = run_json(&temp, &["--json", "sync"]);
    let (status, json, _stderr) = run_json(&temp, &["--json", "projects", "list"]);
    assert_eq!(status, 0);
    let items = json["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    let slugs: Vec<&str> = items
        .iter()
        .map(|item| item["project_slug"].as_str().unwrap())
        .collect();
    assert!(slugs.contains(&PROJECT_ONE_SLUG));
    assert!(slugs.contains(&PROJECT_TWO_SLUG));
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
    assert_eq!(thread["title"], "build a CLI for searching past Claude threads");
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
            "--json",
            "messages",
            "search",
            "Tweet",
            "--role",
            "user",
            "--limit",
            "10",
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
        &[
            "--json",
            "messages",
            "search",
            "Tweet",
            "--role",
            "system",
        ],
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
    assert!(items.iter().any(|item| item["thread_id"] == PROJ_TWO_THREAD));
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
    assert_eq!(stats_json["data"]["thread_count"], 6);
    assert_eq!(stats_json["data"]["subagent_count"], 1);
    assert_eq!(stats_json["data"]["project_count"], 2);

    let (debug_status, debug_json, _stderr) = run_json(&temp, &["--json", "debug", "paths"]);
    assert_eq!(debug_status, 0);
    assert_eq!(debug_json["data"]["projects_root_exists"], true);
    assert_eq!(debug_json["data"]["index_exists"], true);
    assert_eq!(debug_json["data"]["history_exists"], true);
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
    assert_eq!(json["data"]["discovered_files"], 8);

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
    fs::write(&history_path, format!("{original}\nnot-json\n"))
        .expect("write malformed history");

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
    assert!(items.iter().any(|item| item["thread_id"] == PROJ_TWO_THREAD));
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
        &[
            "--json",
            "messages",
            "search",
            "review",
            "--limit",
            "10",
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
