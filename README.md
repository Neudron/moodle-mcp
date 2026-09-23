# moodle-mcp

Moodle course extractor + [Model Context Protocol](https://modelcontextprotocol.io) server, in Rust. Syncs course files into a local tree your agents can read, with full-text search on top.

Two binaries: `moodle-sync` (CLI extractor) and `moodle-mcp` (MCP stdio server with 4 tools).

## Quick install (Arch x86_64, no Rust needed)

```bash
curl -fsSL https://github.com/Neudron/moodle-mcp/releases/download/v0.1.0/install.sh | bash
```

The installer asks for your Moodle URL + webservice token, installs both binaries to `~/.local/bin`, and stores the token in a `0600` file. It never puts secrets in env or history.

## Build from source

```bash
./setup.sh   # asks for URL + token, then cargo build --release
```

Binaries land in `target/release/`.

## Configuration

Copy `.env.example` to `.env` and fill it in — or export the vars. Both binaries load `.env` automatically.

| Var | Required | What |
|-----|----------|------|
| `MOODLE_URL` | yes | Moodle base URL, no trailing slash. No default — the binary refuses to start without it |
| `MOODLE_TOKEN_FILE` | yes | Path to the file holding your webservice token (`profile → Security keys`), mode `0600`, min 16 chars. Path only — the secret itself never goes in env |
| `MOODLE_ROOT` | no | Extraction root (default `./smx`) |
| `MOODLE_OFFLINE` | no | Set to `1` to block all network |
| `MOODLE_*` | no | Tuning overrides, see `.env.example` (`CONCURRENCY`, `API_TIMEOUT_SECS`, …) |

> [!NOTE]
> Auth is token-file only. There is no username/password login, and none is planned — PRs adding one will be closed.

## Usage

```bash
export MOODLE_ROOT="$PWD/smx" MOODLE_URL="https://your-school.edu/moodle"
export MOODLE_TOKEN_FILE="$MOODLE_ROOT/.moodle/token"

moodle-sync --all            # sync every visible course
moodle-sync --course 42      # sync one course
```

Courses land in `$MOODLE_ROOT/<shortname>/RAxx/nnn-module/`. Only `pluginfile.php` URLs are downloaded; everything else becomes a `.url` pointer. Re-syncs skip files whose sha256 matches `state.json`, so they are cheap.

### MCP server

```json
{ "mcpServers": { "moodle": {
    "command": "moodle-mcp",
    "env": { "MOODLE_ROOT": "/path/to/smx", "MOODLE_URL": "https://…" }
} } }
```

Tools: `list_courses` → `course_contents` → `sync_course_tool` / `sync_all`. Same layout and state as the CLI.

## Development

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Tests run against an in-process mock Moodle server — no network needed. CI also runs `cargo audit`.

## Layout

```text
src/moodle.rs   API client (from_env, retry, redaction)
src/errors.rs   redact_token() — keep it covering every dump path
src/config.rs   env > .moodle/config.toml > builtins (never holds the token)
src/sync/       planner + downloader + gc
src/bin/mcp.rs  MCP stdio server   src/bin/sync.rs  CLI
tests/          integration suites + mock server
```
