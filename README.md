# claude-threads

`claude-threads` is a local CLI for querying Claude Code and Claude desktop Cowork session archives with stable JSON, predictable errors, and minimal noise.

It is designed for repeated agent use against local `~/.claude/projects/` and Cowork session data, not as a hosted service or GUI.

> **Scope.** This is a personal tool published for convenience. No support is
> guaranteed and behavior may change between releases.

## What It Does

- Indexes local Claude Code archives from `~/.claude/projects/**/*.jsonl`, including subagents and workflow agents
- Indexes Claude desktop **Cowork** sessions from the app's `local-agent-mode-sessions/` store (all known layouts), with titles, spaces, selected folders, and archived state
- Treats each project (`~/.claude/projects/{cwd-slug}/`, or a Cowork space) as a first-class dimension
- Exposes normalized `projects`, `threads`, `messages`, and `events` reads, filterable by `--source claude-code|cowork`
- Supports exact reads by stable ids after discovery/search
- Keeps machine-readable output stable with `--json`
- Subagent / sidechain threads are indexed but excluded from search by default
- Titles prefer `/rename` custom titles, desktop app titles, and Claude's auto-generated `ai-title`, then fall back to the first real prompt and `~/.claude/history.jsonl`

## Install

### 1. Install the CLI

Pinned to a specific version (reproducible across machines):

```bash
cargo install --git https://github.com/0xmrwn/claude-threads --tag v0.1.0 --locked
```

Always-latest (re-run with `--force` to upgrade in place):

```bash
cargo install --git https://github.com/0xmrwn/claude-threads --tag latest --locked --force
```

The `latest` tag is automatically moved to the newest released commit by
[`.github/workflows/move-latest.yml`](.github/workflows/move-latest.yml)
whenever a release is published or edited.

Both install commands build from source — one-time ~17s compile per machine,
no precompiled binaries. After install, run the CLI normally:

```bash
claude-threads --help
```

### 2. Optional: install the skill so agents discover it

A bundled [`SKILL.md`](skills/recall-claude-threads/SKILL.md) teaches any
skill-aware coding agent (Claude Code, Codex, and 41+ others) how to use the
CLI without having to be pointed at `--help`. Install it globally into both
Claude Code and Codex in one command via [`vercel-labs/skills`][skills]:

```bash
npx skills add 0xmrwn/claude-threads -g -a claude-code -a codex -y
```

This places the skill at `~/.claude/skills/recall-claude-threads/` and
`~/.codex/skills/recall-claude-threads/` (symlinked to a single canonical
copy, so `npx skills update` refreshes both at once). The skill is named
`recall-claude-threads` — distinct from the binary name — because agents load
it when the user says things like *"recall that thread where we…"* or *"what
did we decide about…"*.

The two installs are independent: the CLI works without the skill, and the
skill is just a text file pointing at the CLI, so either can be installed on
its own.

[skills]: https://github.com/vercel-labs/skills

## Common Commands

```bash
claude-threads --json sync
claude-threads --json projects list
claude-threads --json threads list --project /Users/me/Projects/sweatshop --order asc --limit 1
claude-threads --json threads search "build a CLI" --limit 20
claude-threads --json threads search "refactor index" --project /Users/me/Projects/sweatshop
claude-threads --json threads search "review the audit" --include-subagents
claude-threads --json threads list --source cowork --limit 10
claude-threads --json threads list --project "Q3 Planning"
claude-threads --json threads resolve "design doctrine"
claude-threads --json threads read <thread-id>
claude-threads --json messages list --project /Users/me/Projects/sweatshop --role user --order asc --limit 1
claude-threads --json messages search "compaction protocol" --role assistant --limit 20
claude-threads --json messages search "board deck" --source cowork
claude-threads --json messages read <message-id>
claude-threads --json events read <thread-id> --limit 50
claude-threads --json index stats
claude-threads --json debug paths
```

## Behavior

- Source archives are read-only
- The derived index lives under `$CLAUDE_HOME/claude-threads/index.sqlite` or `~/.claude/claude-threads/index.sqlite`; it is rebuilt automatically when its schema version changes
- Cowork sessions are discovered under `$CLAUDE_DESKTOP_HOME/local-agent-mode-sessions/` (default `~/Library/Application Support/Claude` on macOS). Each thread carries `source: "claude_code" | "cowork"`; Cowork threads use `project_slug` `cowork:<space-id>` (space name in `project_name`) or `cowork` for sessions outside a space, plus `app_session_id` and `selected_folders`
- Desktop Code tab metadata (`claude-code-sessions/`) enriches Claude Code threads with the app title, `app_session_id`, and `is_archived`
- Metadata-only changes (renames, archiving, history, agent metadata) re-index just the affected threads
- `sync` is explicit, but read/search/list commands also auto-sync when the index is missing or stale
- If another `claude-threads` process is already syncing, read commands fall back to the current index instead of failing on a write lock
- Subagent files (under `{session-uuid}/subagents/agent-*.jsonl` and `subagents/workflows/{wf_id}/agent-*.jsonl`) are indexed with stable ids of the form `{parent_session_id}:agent:{hash}`, carry `agent_type`/`workflow_id` from their `.meta.json`, and are excluded from default search/list; pass `--include-subagents` to include them
- Message roles reflect authorship: `user` is what the human typed (including prompts queued while the agent was busy), `assistant` is model text, and `system` is harness-injected content with `kind` `task_notification`, `peer_message`, `system_event`, or `away_summary`. Synthetic API-error replies are not indexed
- Transcripts without any user/assistant records (for example `ai-title`-only stubs left by non-persisted `claude -p` runs) are tracked but not indexed as threads; `index stats` reports them as `skipped_empty_files`
- `threads list` and `messages list` provide chronological ordering with `--order asc|desc`; `messages list` also supports `--role user|assistant|system` for questions like "what was my first message in this project?". `threads list` orders by `started_at` with `updated_at` fallback; `messages list` orders by `timestamp`. Rows with null timestamps always sort to the end, regardless of direction
- Threads continued via `/compact` are flagged with `was_compacted: true`; the synthetic compact-summary message is searchable as `kind=compact_summary` but never used as the thread title. Compact-summary `text` is capped at 16,000 bytes in the index — to read the full summary, use `events read <thread-id>` and locate the matching ordinal
- Title derivation order: `/rename` custom title → desktop app title → `ai-title` → subagent spawn description → first real user prompt (skipping `<command-name>`, `<local-command-caveat>`, `isMeta`, and harness messages) → `~/.claude/history.jsonl` → session UUID

## Output Contract

Every `--json` command writes a single JSON envelope to stdout with:

- `schema_version`
- `command`
- `ok`
- `data`
- `meta`
- `error`

Diagnostics and warnings go to stderr.

## Error / Exit Codes

| Exit | Code              | Meaning                                 |
|------|-------------------|-----------------------------------------|
| 0    | _(success)_       | Command succeeded                       |
| 1    | `internal_error`  | Unexpected internal failure             |
| 2    | `usage_error`     | Bad flag or argument                    |
| 3    | `archive_not_found` | Neither `~/.claude/projects/` nor the Cowork store exists |
| 4    | `index_missing`   | Derived index not yet created           |
| 5    | `not_found`       | Exact id or project not found           |
| 6    | `ambiguous`       | Reference matched multiple candidates   |
| 7    | `sync_failed`     | Parsing a source jsonl file failed      |

## Development

```bash
cargo test
cargo fmt
```

End-to-end tests live in `tests/cli.rs` and run against fixtures under `tests/fixtures/claude-home/` (Claude Code) and `tests/fixtures/claude-desktop/` (Cowork and desktop Code tab metadata).
