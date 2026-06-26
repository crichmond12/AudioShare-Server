# Superpowers development lifecycle — where everything lives

This project uses the **superpowers** workflow (brainstorm → write plan →
subagent-driven execution → review → finish). This page is a map of every file
the workflow reads or writes, in the order they appear in a feature's life. Use
it to find the spec, the plan, and — what you most often want — the **review
documents** for a change.

All paths are relative to the repo root (`/Users/christianrichmond/audio_share`).

---

## 1. Specs (the design, committed)

`docs/superpowers/specs/YYYY-MM-DD-<topic>-design.md`

The output of the **brainstorming** skill: the agreed design before any code.
Locked decisions, architecture, wire-protocol additions, and the slice
breakdown live here. Committed to git and meant to be long-lived.

- Current AirPlay design: `docs/superpowers/specs/2026-06-23-airplay-receiver-design.md`
  (Slice 3's "as-planned decisions" note was appended here before implementation.)

## 2. Plans (the step-by-step, committed)

`docs/superpowers/plans/YYYY-MM-DD-<feature>.md`

The output of the **writing-plans** skill: one spec becomes an ordered list of
bite-sized, test-driven tasks, each with exact files, complete code, and the
exact test commands. This is the script the execution phase follows.

- `docs/superpowers/plans/2026-06-25-airplay-receiver-slice-3.md` (in progress)
- `docs/superpowers/plans/2026-06-23-airplay-receiver-slice-2.md`
- `docs/superpowers/plans/2026-06-23-airplay-receiver-slice-1.md`

## 3. Execution scratch + review documents (NOT committed)

`.superpowers/sdd/` — git-ignored working directory for the
**subagent-driven-development** skill. This is where the per-task review trail
lives. It is scratch, so it is not in git history; if you want a durable record,
read it before the branch is finished and cleaned.

Per task **N** you will find:

| File | What it is |
|---|---|
| `task-N-brief.md` | The task's full text sliced out of the plan — the exact requirements the implementer subagent was handed. |
| `task-N-report.md` | The implementer's report: what it built, **TDD red/green evidence**, files changed, self-review findings, concerns. (Task 4's was reconstructed by the controller after a mid-run connection drop — noted at the top of that file.) |
| `review-<base7>..<head7>.diff` | The **review package**: commit list + `git diff --stat` + full diff with context for that task. This is the exact artifact each task reviewer read. |

And one file spanning the whole feature:

| File | What it is |
|---|---|
| `progress.md` | The **progress ledger** — the source of truth for what's done. One block per task: commit SHA, spec verdict (✅/❌), quality verdict (Approved/Needs fixes), deviations, and **deferred Minor findings** earmarked for the final review. Survives context loss; recovery map after any interruption. |

> **Where are the reviewers' written verdicts?** The task reviewers are
> subagents; their full verdict (spec compliance, strengths, Critical/Important/
> Minor issues, assessment) is returned to the controller and **summarized into
> `progress.md`** rather than saved as its own file. So to review the change
> history, read **`.superpowers/sdd/progress.md`** (verdicts + deferred findings)
> alongside each **`review-*.diff`** (the actual code reviewed) and
> **`task-N-report.md`** (test evidence). The final whole-branch review (step 5)
> produces the last, broadest verdict.

### How to read the review trail quickly

```bash
# The verdict + deferred-findings ledger for the whole feature:
cat .superpowers/sdd/progress.md

# Everything reviewed for one task (diff + test evidence + requirements):
cat .superpowers/sdd/review-*..*.diff      # the diffs, one per task
cat .superpowers/sdd/task-4-report.md       # one task's implementer report
cat .superpowers/sdd/task-4-brief.md        # one task's requirements

# The committed history is the other durable record:
git log --oneline master..airplay-slice-3
```

## 4. Git branch + commits (committed)

Each feature runs on its own branch (`airplay-slice-3` here), one commit per
task, each message prefixed with the slice. The branch is merged `--no-ff` into
`master` at the end. The commit trail is the durable mirror of the ledger — if
`.superpowers/sdd/` is ever cleaned, `git log` reconstructs progress.

## 5. Final review + finish (end of feature)

- **Final whole-branch review:** one more review package over the entire branch
  (`review-package <merge-base> HEAD`), read by a final reviewer on the most
  capable model. Its verdict and any fix wave are recorded at the bottom of
  `progress.md`.
- **finishing-a-development-branch** skill: merges to `master` (or opens a PR),
  deletes the feature branch, and records the merge commit in the ledger.

---

## One-line summary

- **Design** → `docs/superpowers/specs/` (committed)
- **Plan** → `docs/superpowers/plans/` (committed)
- **Per-task requirements / test evidence / reviewed diff** → `.superpowers/sdd/task-N-brief.md`, `task-N-report.md`, `review-*.diff` (scratch, git-ignored)
- **Verdicts + deferred findings (the review record)** → `.superpowers/sdd/progress.md` (scratch, git-ignored)
- **Durable history** → git branch commits, merged to `master`
