---
name: recall-claude-threads
description: >-
  List, search, resolve, and read past Claude Code and Claude desktop Cowork
  conversation threads from local archives (~/.claude/projects/ plus the
  desktop app's Cowork session store) via the `claude-threads` CLI. Use
  whenever you want to find an earlier Claude session by topic, recover
  decisions made in prior threads, mine successful past work for reusable
  patterns, or pull the first/last message in a project in chronological
  order. Returns stable JSON with `--json`. Read-only over the source archives.
---

# recall-claude-threads

`claude-threads` is a local CLI that indexes Claude Code transcripts
(`~/.claude/projects/**/*.jsonl`, including subagents and workflow agents) and
Claude desktop Cowork sessions into a SQLite + FTS5 derived index and exposes
precise read/search/list commands with a deterministic JSON envelope. Use it to find prior conversations
by topic, to look up exactly what was decided in a past thread, or to mine old
sessions for reusable patterns — without loading raw transcripts into context.

## When to use

- User says "find that thread where we…", "what did we decide about…",
  "remember the session from last week about…", "show me past work on…",
  "recall the conversation about…", "that Cowork session where…"
- User asks "what was my first/last message in this project?" or wants
  threads ordered chronologically (oldest-first or newest-first)
- Before resuming work on a topic, to recover context from a prior session
- To quote a specific past message by exact id
- To inspect the full event stream of one past thread

## Core conventions

- **Always pass `--json`** unless a human-readable dump is actually wanted.
  Every read/search/list returns a stable envelope:
  `{ schema_version, command, ok, data, meta, error }`.
- **IDs are stable.** `thread_id` is the session UUID for main threads, or
  `{parent_session_id}:agent:{agent_hash}` for subagents. `message_id` is
  `{thread_id}:m:{ordinal}`. `event_id` is `{thread_id}:e:{ordinal}`.
- **Exit codes are stable.** `0` ok, `2` usage error, `3` archive not found,
  `4` index missing, `5` not found, `6` ambiguous, `7` sync failed.
- **Default scope excludes subagents.** Pass `--include-subagents` to include
  sidechain / `Task` tool spawns and workflow agents in search and list results.
- **Two sources.** Every thread, search hit, and message carries
  `source: "claude_code" | "cowork"`. Filter with `--source claude-code` or
  `--source cowork` on `threads list|search` and `messages list|search`.
- **Roles reflect authorship.** `role=user` is what the human typed (including
  prompts queued while the agent was busy); `role=assistant` is model text;
  `role=system` is harness-injected content (`kind` = `task_notification`,
  `peer_message`, `system_event`, or `away_summary` recaps). Use
  `--role user` for "what did I ask" questions.
- **Lazy auto-sync.** Read, search, and list commands sync the index
  automatically when stale. Explicit `sync` is only needed before measuring
  `index stats`.
- **Chronological listing is first-class.** `threads list` orders by
  `started_at` with `updated_at` fallback and stable `thread_id` tiebreaks.
  `messages list` orders by `timestamp`, then `thread_id`, then message
  ordinal, and supports `--role user|assistant|system` for first/last message
  queries inside a project. Rows with null timestamps always sort to the
  end regardless of direction.

## Command surface

```text
Query, search, resolve, and read local Claude Code and Claude desktop Cowork thread archives with
deterministic JSON, predictable errors, and agent-friendly subcommands.

Usage: claude-threads [OPTIONS] <COMMAND>

Commands:
  sync      Refresh the local derived index from Claude Code and Cowork archives
  projects  List indexed projects
  threads   List, search, resolve, and read normalized threads
  messages  List, search, and read normalized messages
  events    Read normalized event streams for a thread
  index     Inspect index statistics
  debug     Show resolved archive and index paths
  help      Print this message or the help of the given subcommand(s)

Options:
      --json
          Emit machine-readable JSON to stdout

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

Shorthand of the most useful invocations:

```text
claude-threads --json sync [--rebuild]
claude-threads --json projects list [--limit 50]
claude-threads --json threads list [--project <slug-cwd-or-space>] [--source claude-code|cowork] [--order asc|desc] [--limit 20] [--include-subagents]
claude-threads --json threads search <query> [--limit 20] [--project <slug>] [--source claude-code|cowork] [--include-subagents]
claude-threads --json threads resolve <ref>
claude-threads --json threads read <thread-id>
claude-threads --json messages list [--project <slug-cwd-or-space>] [--role user|assistant|system] [--source claude-code|cowork] [--order asc|desc] [--limit 20] [--include-subagents]
claude-threads --json messages search <query> [--limit 20] [--project <slug>] [--role user|assistant|system] [--source claude-code|cowork] [--include-subagents]
claude-threads --json messages read <message-id>
claude-threads --json events read <thread-id> [--limit 50]
claude-threads --json index stats
claude-threads --json debug paths
```

## Examples

Find a past thread by topic:

```bash
claude-threads --json threads search "build a CLI"
```

Narrow to one project (slug or full cwd both work; slugs start with `-`):

```bash
claude-threads --json threads search "refactor index" --project -Users-me-Projects-sweatshop
claude-threads --json threads search "refactor index" --project /Users/me/Projects/sweatshop
```

Find a Cowork conversation, or list the sessions in one Cowork space (project
name, space id slug `cowork:<space-id>`, or `cowork` for sessions outside a space):

```bash
claude-threads --json threads search "board deck" --source cowork
claude-threads --json threads list --project "Q3 Planning" --limit 20
```

List threads in one project, oldest-first:

```bash
claude-threads --json threads list --project /Users/me/Projects/sweatshop --order asc --limit 20
```

Find the first user message ever sent in one project:

```bash
claude-threads --json messages list --project /Users/me/Projects/sweatshop --role user --order asc --limit 1
```

Read the full normalized record for one thread:

```bash
claude-threads --json threads read 8069f1c6-8305-4f3d-bc31-6ec5230f8921
```

Resolve a fuzzy reference. On a single hit the thread comes back; on multiple
matches, exit code `6` is returned with a `candidates` array in `error.details`:

```bash
claude-threads --json threads resolve "design doctrine"
```

Find a specific message across all threads, then read it exactly:

```bash
claude-threads --json messages search "compaction protocol" --role assistant --limit 5
claude-threads --json messages read 8069f1c6-8305-4f3d-bc31-6ec5230f8921:m:42
```

Walk the raw event stream for one thread (full record payloads, byte-accurate):

```bash
claude-threads --json events read 8069f1c6-8305-4f3d-bc31-6ec5230f8921 --limit 50
```

Discover what projects exist in the index, most recently active first:

```bash
claude-threads --json projects list
```

Inspect index health and source paths:

```bash
claude-threads --json index stats
claude-threads --json debug paths
```

## Composing with jq

Search hits return small records — pipe through `jq` to grab just what you need:

```bash
# Get the top thread id for a query
claude-threads --json threads search "rework plan" --limit 1 | jq -r '.data.items[0].thread_id'

# Get all message ids that match a phrase
claude-threads --json messages search "linear webhook" --limit 20 | jq -r '.data.items[].message_id'

# Pull the earliest user message in one project
claude-threads --json messages list --project /Users/me/Projects/sweatshop --role user --order asc --limit 1 | jq '.data.items[0] | {message_id, thread_id, text}'

# Get the title and project_cwd for a thread you already know
claude-threads --json threads read <thread-id> | jq '.data.thread | {title, project_cwd, was_compacted}'
```

## Things worth knowing

- **Subagents are physically separate** under `{session-uuid}/subagents/agent-*.jsonl`
  (workflow agents under `subagents/workflows/{wf_id}/`). Their `thread_id` is
  `{parent_session_id}:agent:{hash}` so `threads read` works; `agent_type`,
  `workflow_id`, and the spawn description (used as title) come from the
  agent's `.meta.json`.
- **Cowork sessions** live under the desktop app's `local-agent-mode-sessions/`
  store. Their `thread_id` is the transcript session UUID, `app_session_id` is
  the desktop `local_…` id, `project_slug` is `cowork:<space-id>` (with the
  space name in `project_name`) or `cowork`, and `selected_folders` lists the
  folders the session was given. Desktop Code tab sessions also get
  `app_session_id` and `is_archived` from the app's metadata.
- **Compact summaries** (`/compact` continuation messages) are indexed as
  `kind=compact_summary` and excluded from title derivation. The thread row
  exposes `was_compacted: true` so the conversation is known to chain back to
  earlier context. The stored `text` is **capped at 16,000 bytes** — `messages
  read` on a compact-summary message returns the truncated form. To recover the
  full summary, use `events read <thread-id>` and locate the matching ordinal.
- **Tool calls and `thinking` blocks are not indexed as messages in v1.** They
  exist in the `events` table only. Use `events read <thread-id>` to walk
  full payloads.
- **Title derivation** prefers explicit titles: `/rename` custom titles, then
  the desktop app's session title, then Claude's auto-generated `ai-title`,
  then (for subagents) the spawn description, then the first non-noise user
  message, then `~/.claude/history.jsonl`, then the sessionId. All title
  candidates stay searchable.
- **Empty stubs are skipped.** Transcripts with no user/assistant records
  (e.g. `ai-title`-only files left by non-persisted `claude -p` runs) are not
  threads; `index stats` reports them as `skipped_empty_files`.
- **Source archives are read-only.** The derived index lives at
  `$CLAUDE_HOME/claude-threads/index.sqlite` (default `~/.claude/claude-threads/`).
  `CLAUDE_DESKTOP_HOME` overrides the desktop app data directory used to find
  Cowork sessions (default `~/Library/Application Support/Claude` on macOS).
- **Concurrency.** If another `claude-threads` process holds the SQLite write
  lock, read commands fall back to the existing index instead of failing.
