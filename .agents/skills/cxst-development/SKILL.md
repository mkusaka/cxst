---
name: cxst-development
description: Use when Codex is modifying, testing, documenting, or reviewing this cxst repository. Trigger for implementation changes, README or skill updates, release preparation, test planning, secret-safety checks, and keeping repo-local developer guidance aligned with the user-facing cxst skill.
---

# cxst Development

Use this skill when working on the `cxst` repository itself. This is developer
guidance for agents changing the repo, not end-user guidance for running the
CLI. Keep user-facing usage instructions in `skills/cxst`.

## Change Workflow

Before changing behavior, read the relevant code, tests, `README.md`, and
`skills/cxst/SKILL.md`. Do not infer Codex protocol, auth, or rate-limit
behavior without checking the implementation or fixtures in this repo.

When changing `cxst` behavior, update related documentation in the same change:

- `README.md` for human-facing CLI documentation
- `skills/cxst/SKILL.md` for installable user-facing skill guidance
- `skills/cxst/agents/openai.yaml` when skill metadata becomes stale

Keep command names, options, output fields, JSON shape, exit codes, unavailable
rate-limit behavior, and fixture assumptions aligned across code, tests, README,
and skills.

## Safety

Do not add local machine names, personal account details, private organization
names, tokens, refresh tokens, API keys, account IDs, raw authorization headers,
cookies, or raw backend identifiers to committed files.

Use generic examples for paths, accounts, homes, and test fixtures. Prefer
fixture data that is synthetic and clearly anonymous.

Before committing, run the repo-required staged diff and staged file content
secret scan. If the scanner is unavailable or fails, stop instead of replacing
it with a weaker ad hoc scan.

## Verification

For Rust changes, follow the repository's existing commands and scope tests to
the changed crate when possible. At minimum, verify formatting and relevant
tests before reporting completion.

For documentation-only changes, validate the skill folder when a skill changed
and check for whitespace errors.
