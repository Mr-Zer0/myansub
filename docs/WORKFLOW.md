# Development Workflow (Claude Code)

## 1. Roles

| Role | Who | Responsibilities |
|---|---|---|
| Orchestrator | Main Claude Code session | Plans tasks from `MILESTONES.md`, delegates, decides on findings, owns git, keeps docs consistent, talks to the product owner |
| Developer | `rust-tdd-developer` subagent | Implements one task test-first; reports evidence |
| Spec QA | `qa-spec-reviewer` subagent | Requirements, acceptance criteria, CLI/data-model conformance, test quality |
| Reliability QA | `qa-reliability-reviewer` subagent | Crash safety, resumability, async/concurrency, errors, security |
| LLM QA | `qa-llm-reviewer` subagent | Gemini integration, prompt stability, caching, retries, cost, Burmese output checks |

Reviewers are read-only. Subagents cannot ask the product owner questions; they return
questions to the orchestrator, which asks or decides.

## 2. Task loop

1. **Plan.** Pick the next small group of acceptance criteria (typically 1–3 ACs from one
   milestone). Write a task brief (§4).
2. **Implement.** Delegate the brief to `rust-tdd-developer`.
3. **Review.** Run the applicable reviewers in parallel (§3), giving each the brief and the
   developer's report.
4. **Fix.** If any reviewer returns `BLOCK`, or reports `major` findings the orchestrator
   accepts, send those findings verbatim to `rust-tdd-developer` as a fix task. Where a
   finding is testable, the fix starts with a failing test that reproduces it.
   Re-run only the reviewers whose findings were addressed.
5. **Escalate.** After 3 fix rounds on the same task, stop and ask the product owner.
6. **Accept.** Orchestrator verifies `fmt`, `clippy`, `test` locally, then commits:
   `feat(m4): project resolution (M4-AC4, M4-AC5)`.

A milestone is complete when every AC ID has a tagged test (or is marked manual), and a
final review of the whole milestone diff by all applicable reviewers returns no blockers.

## 3. Which reviewers to run

| Change touches | Spec | Reliability | LLM |
|---|---|---|---|
| anything | ✓ | | |
| `store/`, `pipeline/`, `review/`, `project.rs`, file I/O, `main.rs`/`cli.rs` | ✓ | ✓ | |
| `formats/` | ✓ | ✓ | |
| `llm/`, `quality/`, `pipeline/analysis.rs`, `pipeline/translation.rs` | ✓ | ✓ | ✓ |
| docs only | ✓ | | |

## 4. Task brief template

```
Task: <short title>
Milestone / ACs: M4-AC4, M4-AC5
Spec references: CLI.md §2; ARCHITECTURE.md §2 (Project resolution)
Scope: src/project.rs, src/cli.rs (run FILE), tests/project_resolution.rs
Out of scope: <what not to touch>
Notes: <decisions already made, known pitfalls>
Base for review: <commit hash the diff is measured from>
```

## 5. Handling disagreement

The developer may dispute a finding with reasoning in its report. The orchestrator decides.
If the decision changes architecture or a documented rule, record it in `DECISIONS.md`
in the same commit.
