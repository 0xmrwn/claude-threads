# claude-threads

`claude-threads` is a local CLI for querying Claude Code session archives with stable JSON, predictable errors, and minimal noise.

It is designed for repeated agent use against local `~/.claude/projects/` data, not as a hosted service or GUI.

> **Scope.** This is a personal tool published for convenience. No support is
> guaranteed and behavior may change between releases.

## What It Does

- Indexes local Claude Code archives from `~/.claude/projects/**/*.jsonl`
- Treats each project (`~/.claude/projects/{cwd-slug}/`) as a first-class dimension
- Exposes normalized `projects`, `threads`, `messages`, and `events` reads
- Supports exact reads by stable ids after discovery/search
- Keeps machine-readable output stable with `--json`
- Subagent / sidechain threads are indexed but excluded from search by default
- Title derivation strips slash-command and synthetic noise; enriched from `~/.claude/history.jsonl`

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
claude-threads --json threads resolve "design doctrine"
claude-threads --json threads read <thread-id>
claude-threads --json messages list --project /Users/me/Projects/sweatshop --role user --order asc --limit 1
claude-threads --json messages search "compaction protocol" --role assistant --limit 20
claude-threads --json messages read <message-id>
claude-threads --json events read <thread-id> --limit 50
claude-threads --json index stats
claude-threads --json debug paths
```

## Behavior

- Source archives are read-only
- The derived index lives under `$CLAUDE_HOME/claude-threads/index.sqlite` or `~/.claude/claude-threads/index.sqlite`
- `sync` is explicit, but read/search/list commands also auto-sync when the index is missing or stale
- If another `claude-threads` process is already syncing, read commands fall back to the current index instead of failing on a write lock
- Subagent files (under `{session-uuid}/subagents/agent-*.jsonl`) are indexed with stable ids of the form `{parent_session_id}:agent:{hash}` and excluded from default search/list; pass `--include-subagents` to include them
- `threads list` and `messages list` provide chronological ordering with `--order asc|desc`; `messages list` also supports `--role user|assistant` for questions like "what was my first message in this project?". `threads list` orders by `started_at` with `updated_at` fallback; `messages list` orders by `timestamp`. Rows with null timestamps always sort to the end, regardless of direction
- Threads continued via `/compact` are flagged with `was_compacted: true`; the synthetic compact-summary message is searchable as `kind=compact_summary` but never used as the thread title. Compact-summary `text` is capped at 16,000 bytes in the index — to read the full summary, use `events read <thread-id>` and locate the matching ordinal
- Title derivation skips `<command-name>`, `<local-command-caveat>`, and `isMeta` records, then falls back to `~/.claude/history.jsonl` enrichment, then to the session UUID

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
| 3    | `archive_not_found` | `~/.claude/projects/` is missing      |
| 4    | `index_missing`   | Derived index not yet created           |
| 5    | `not_found`       | Exact id or project not found           |
| 6    | `ambiguous`       | Reference matched multiple candidates   |
| 7    | `sync_failed`     | Parsing a source jsonl file failed      |

## Development

```bash
cargo test
cargo fmt
```

End-to-end tests live in `tests/cli.rs` and run against fixtures under `tests/fixtures/claude-home/`.
