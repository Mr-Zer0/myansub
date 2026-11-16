# Review Protocol (all QA reviewers)

## 1. Inputs

The orchestrator gives you: the task brief, the developer's report, and the base commit.
Review the diff from the base commit: `git diff <base>` plus untracked files from
`git status --porcelain`. If no base is given, use `git diff HEAD` and untracked files.

## 2. Ground rules

- Read-only. Never edit files, never run commands that modify the tree
  (`cargo fmt` without `--check`, `git checkout`, `git stash`, etc.).
- Judge against the docs, not personal taste. Cite the rule: `ARCHITECTURE.md §6 rule 1`.
- Every finding needs a concrete scenario or evidence ("if the process is killed after line 84,
  the batch result is lost because…"). No speculative findings.
- Do not report what `rustfmt` or `clippy -D warnings` already enforce.
- Stay in your scope. If you notice something outside it that looks serious, list it under
  "Outside my scope" in one line; do not analyze it.
- Do not rewrite code. Describe the fix in words; a short snippet (≤ 5 lines) is allowed.

## 3. Severity

| Severity | Meaning | Effect |
|---|---|---|
| `blocker` | Violates a requirement, acceptance criterion, crash-safety rule, or CLAUDE.md non-negotiable; or a demonstrable bug | Verdict `BLOCK` |
| `major` | Likely bug, missing test for a stated behavior, or significant spec gap | Orchestrator decides |
| `minor` | Clarity, naming, small robustness improvement | Optional |

Verdict: `BLOCK` if any blocker; `PASS_WITH_NOTES` if majors or minors only; otherwise `PASS`.

## 4. Report format

```
## Verdict: PASS | PASS_WITH_NOTES | BLOCK
## Scope reviewed
<files / ACs / areas>
## Findings
| # | Severity | Location | Rule | Problem and scenario | Suggested fix |
|---|---|---|---|---|---|
## Checks run
<commands and results>
## Outside my scope
## Could not verify
```
