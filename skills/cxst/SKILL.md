---
name: cxst
description: Use when Codex needs to inspect local Codex account status, active configuration, account token activity, or 5-hour/weekly/monthly rate-limit remaining usage with the cxst CLI instead of opening the Codex TUI. Trigger for requests to check Codex status, `/usage` token activity, remaining usage, rate limits, active model/provider, auth state, Codex home, permissions, JSON status output, preflight-check remaining usage, or wait until remaining rate-limit usage reaches a threshold.
---

# cxst Status

Use `cxst` to inspect local Codex status and account usage from a shell. Prefer
it when the user wants a quick status snapshot, token activity, or
machine-readable status without opening an interactive TUI.

## Commands

Human-readable status:

```sh
cxst
```

Machine-readable status:

```sh
cxst --json
```

Account token activity from the same source as TUI `/usage`:

```sh
cxst usage daily
cxst usage weekly --json
cxst usage cumulative
```

One-shot threshold check for automation preflight:

```sh
cxst check --remaining-percent 10 --window both
```

Wait until selected remaining usage reaches a threshold:

```sh
cxst wait --remaining-percent 10 --window both --interval 60s
```

Use `--window 5h`, `--window weekly`, `--window monthly`, or `--window all` to
choose which limit window to monitor. `both` remains accepted as a backwards
compatible alias for all windows. `--remaining-percent` is a remaining-usage
threshold: the command exits when any selected window is at or below that value.
Use `--timeout` when a successful no-trigger exit is needed.

Exit codes:

`cxst check`:

- `0`: selected rate limits are above the threshold
- `1`: threshold reached, or rate-limit status is unavailable

`cxst wait`:

- `0`: timeout reached before the threshold was hit
- `1`: threshold reached, or rate-limit status is unavailable

For token activity, use `daily`, `weekly`, or `cumulative`; `day` and `week`
are accepted aliases. Human output renders a TUI-style 52-week activity chart:
daily is a calendar heatmap, while weekly and cumulative are 7-row bar charts
with scale captions. `cxst usage --json` prints a status object with `summary`
and `dailyUsageBuckets`. It exits successfully even when the backend usage
profile is unavailable, and reports `status: "unavailable"` plus a short
`reason`.

For automation, combine `--json` with the same check or wait options. Check and
wait JSON events include `status`, `thresholdRemainingPercent`, selected
`windows`, optional `reason`, and, for waiting events, optional
`nextPollSeconds`.

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
- 5-hour, weekly, and monthly rate-limit remaining percentages
- reset timestamps or reset display times when available
- account-level token activity summary and daily usage buckets from `/usage`

If rate limits are unavailable, report the short reason from `cxst` and avoid
speculating from lower-level auth or backend errors.

If usage is unavailable, report the short reason from `cxst usage` and avoid
printing raw backend errors. The `tokens` field in usage output is a usage
count, not an auth token. Do not expose auth tokens, refresh tokens, API keys,
account IDs, workspace/profile IDs, raw backend payloads, headers, cookies, or
raw backend identifiers.
