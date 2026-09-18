# moodle-mcp

Moodle extraction MCP server + standalone sync CLI for Institut de Palamós.
Lists courses, dumps course structure, and extracts course files into a
RA-organized local tree (`$MOODLE_ROOT/<shortname>/RAxx/nnn-module/`).

## Quickstart (5 commands to first sync)

```bash
mkdir -p smx/.moodle && chmod 700 smx/.moodle
printf '%s' "$MOODLE_TOKEN" > smx/.moodle/token && chmod 600 smx/.moodle/token
export MOODLE_ROOT="$PWD/smx" MOODLE_URL="https://educaciodigital.cat/iespalamos/moodle"
cargo run -q -p moodle-mcp --bin moodle-sync -- --all
ls "$MOODLE_ROOT"
```

## Environment

| Var | Default | Description |
|---|---|---|
| `MOODLE_URL` | `https://educaciodigital.cat/iespalamos/moodle` | Moodle base URL (https; plain-http prints a warning) |
| `MOODLE_TOKEN_FILE` | `$MOODLE_ROOT/.moodle/token` | File holding the webservice token (should be mode 600, min 16 chars) |
| `MOODLE_ROOT` | `./smx` (see `Moodle::moodle_root`) | Extraction root; state lives in `$MOODLE_ROOT/.moodle/state.json` |
| `RUST_LOG` | `info` | Tracing filter, e.g. `RUST_LOG=debug` or `RUST_LOG=moodle_mcp=debug` |

Token handling: the token is read from the token *file* (never argv), is
only ever sent as a webservice `wstoken` form field or `pluginfile.php`
`token` query param, and must never appear in logs, errors or dumps. Rotate
it from your Moodle profile → Security keys when it leaks or expires (an
expired token surfaces as an `AUTH` error with a "renew token" hint).

## Layout

```text
$MOODLE_ROOT/
  <shortname>/                 # sanitized course shortname, e.g. MP05/
    index.md                   # course summary + file table
    RA01-<topic>/              # section folder from the RA detector
      section-summary.md
      001-<module>/            # per-module folder
        README.md              # module description as markdown
        <file>...              # downloads (sanitized names)
        <link>.url             # external (non-pluginfile) links as pointers
    99-misc/                   # sections with no detectable RA
    00-intro/                  # first section fallback
  .moodle/
    state.json                 # incremental-sync state (sha256 per fileurl)
```

Only `pluginfile.php` URLs are ever downloaded; anything else becomes a
`.url` pointer file. Re-syncs skip files whose sha256 matches state.

## MCP client snippets

Stdio server (`moodle-mcp` bin). Claude Desktop `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "moodle": {
      "command": "/path/to/moodle-mcp/target/debug/moodle-mcp",
      "env": {
        "MOODLE_ROOT": "/path/to/smx",
        "MOODLE_URL": "https://educaciodigital.cat/iespalamos/moodle"
      }
    }
  }
}
```

Tools (4, frozen): `list_courses` → `course_contents {course_id}` →
`sync_course_tool {course_id}` (or `sync_all`). Example:

```json
{ "tool": "course_contents", "args": { "course_id": 123 } }
```

## TUI keys (planned — Task 8, mouse-only)

The TUI is not implemented yet. Design: every action clickable with the
mouse, alternate screen + mouse capture, 100 ms tick, no keyboard required.
Planned tap targets per row: `[Sync]` on courses, `[↓]`/`[👁]` on files,
tab bar `[Courses][Detail][Preview][Search][Jobs][?]`, plus an on-screen
QWERTY for search input. Keyboard shortcuts (where listed in the plan) are
secondary to their clickable equivalents.

## CLI

```bash
moodle-sync --all            # sync every visible course
moodle-sync --course <id>    # sync one course
```

Exit codes: `0` ok · `1` partial · `2` usage/auth · `3` network · `4` config
(full matrix lands with the clap rewrite, Task 9).

## Troubleshooting

| Symptom | Fix |
|---|---|
| `reading token file …` / `does not look like a token` | Create `$MOODLE_ROOT/.moodle/token` (mode 600, ≥16 chars) or set `MOODLE_TOKEN_FILE` |
| `AUTH` / 401 loop | Token expired or revoked → renew in Moodle Security keys, replace token file |
| `moodle http …` / timeouts | Check `MOODLE_URL`, proxy (`HTTPS_PROXY`), retry; per-op timeouts + retry arrive in Task 2 |
| `course <id> not found` | Re-run `list_courses`; ids change on course restore/duplication |
| Empty course dir after sync | Course has no downloadable content yet; run `course_contents` to inspect |

## Development

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Tests run against an in-process mock Moodle server
(`tests/integration/mock.rs`, 50 files + 404/401/429/500 modes); no network
needed. Full roadmap: `docs/superpowers/plans/2026-09-18-moodle-mcp-ultra-1200.md`.
