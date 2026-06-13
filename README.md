# cxst

`cxst` shows safe Codex account and rate-limit status without opening the
Codex TUI.

It mirrors the TUI `/status` rate-limit source by using Codex auth/config and
the Codex backend rate-limit snapshot. It intentionally does not show tokens,
refresh tokens, API keys, account IDs, raw limit IDs, or thread-specific TUI
state.

## Usage

```sh
cxst
cxst --json
cxst -c model=gpt-5.5
```

The default output is a short human-readable status:

```text
Codex status
  Model              gpt-5.5 (reasoning high, summaries detailed)
  Directory          /Users/example/src/project
  Codex home         /Users/example/.codex
  Permissions        Custom (danger-full-access, Approve for me)
  Agents.md          /Users/example/.codex/AGENTS.md
  Account            user@example.com (pro)
  Collaboration mode Default

Rate limits
  5h limit          [████████████████░░░░]  83% left (resets 19:31)
  Weekly limit      [███████░░░░░░░░░░░░░]  33% left (resets 23:37 on 16 Jun)
```

`--json` prints machine-readable JSON with the same core fields plus raw
timestamps and window metadata when available.

## Scope

Included:

- auth/account status as `chatgpt`, `api_key`, `unauthenticated`, or similar
- active Codex model/provider, working directory, permission summary, Codex
  home, collaboration mode, and discovered AGENTS.md source paths
- 5-hour and weekly remaining percentages from the rate-limit snapshot
- reset timestamps and window length when returned by the backend

Not included:

- current TUI thread name/id, fork metadata, or context/token usage
- non-standard profile state such as `CODEX_PROFILE`
- raw backend token values, account IDs, API keys, or raw per-limit identifiers
- automatic watch mode

When rate limits cannot be read, `cxst` prints a short unavailable reason. API
key auth is expected to be unavailable for rate-limit reads because Codex
backend rate limits require ChatGPT/Codex backend auth.

## Development

```sh
cargo fmt --check
cargo test
cargo run -- --json
```
