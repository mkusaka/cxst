---
name: cxst
description: Use when Codex needs to inspect local Codex account status, active configuration, or 5-hour/weekly rate-limit remaining usage with the cxst CLI instead of opening the Codex TUI. Trigger for requests to check Codex status, remaining usage, rate limits, active model/provider, auth state, Codex home, permissions, JSON status output, or to wait until remaining rate-limit usage reaches a threshold.
---

# cxst Status

Use `cxst` to inspect local Codex status from a shell. Prefer it when the user
wants a quick status snapshot or machine-readable status without opening an
interactive TUI.

## Commands

Human-readable status:

```sh
cxst
```

Machine-readable status:

```sh
cxst --json
```

Wait until selected remaining usage reaches a threshold:

```sh
cxst wait --remaining-percent 10 --window both --interval 60s
```

Use `--window 5h`, `--window weekly`, or `--window both` to choose which limit
window to monitor. `--remaining-percent` is a remaining-usage threshold: the
command exits when any selected window is at or below that value. Use
`--timeout` when a successful no-trigger exit is needed.

Exit codes for `cxst wait`:

- `0`: timeout reached before the threshold was hit
- `1`: threshold reached, or rate-limit status is unavailable

For automation, combine `cxst wait --json` with the same wait options. JSON
events include `status`, `thresholdRemainingPercent`, selected `windows`,
optional `reason`, and optional `nextPollSeconds`.

## Alternate Codex Homes

`cxst` follows Codex's standard home-directory environment handling. To inspect
a different Codex home, set the standard Codex home environment variable
explicitly for the command:

```sh
CODEX_HOME=/path/to/codex-home cxst --json
```

Do not infer or document local wrapper conventions. If a user's shell aliases,
functions, or launcher scripts select a Codex home, treat that behavior as
outside `cxst` and verify the effective home from the `codexHome` field.

## Reading Results

Use the output fields as reported by `cxst`:

- account/auth status and plan when available
- active model and provider
- working directory and Codex home
- permission summary
- configured instruction source paths
- 5-hour and weekly rate-limit remaining percentages
- reset timestamps or reset display times when available

If rate limits are unavailable, report the short reason from `cxst` and avoid
speculating from lower-level auth or backend errors.
