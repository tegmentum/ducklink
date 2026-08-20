# Worktree-staleness investigation

**Question.** Why did agent worktrees for this project (e.g. `agent-a5900b2615d365b02`, `agent-a35228cc8fa735f51`) get created from commit `cf496693` instead of current local `main`?

## 1. Cause

Claude Code's `isolation: "worktree"` spawn creates the worktree branch from **`origin/main`**, not from local `main`. When local `main` has moved ahead of `origin/main` (unpushed commits), the worktree starts on the last-pushed tip — which was `cf496693` for this repo when those agents ran.

## 2. Evidence

- `git worktree list` shows `agent-aabb212b11d343f29` sitting at `[cf496693]` — the leftover stale worktree.
- Reflog for every recently-created agent branch identifies its origin explicitly:

  ```
  $ git reflog show worktree-agent-aabb212b11d343f29
  cf496693 ...@{0}: branch: Created from origin/main
  $ git reflog show worktree-agent-a0815bee8125f90f3
  a1382759 ...@{0}: merge main: Fast-forward
  cf496693 ...@{1}: branch: Created from origin/main
  ```

  The `branch: Created from origin/main` line is git's own message for `git branch <name> origin/main` / `git worktree add -b <name> <path> origin/main`.

- `git rev-parse origin/main` = `cf496693…`; `git rev-parse main` = `a1382759…`. Local `main` is well ahead of `origin/main` (multiple unpushed agent-merge commits: `26114462`, `b6e47534`, `af56644d`, `12797098`, ..., up to `a1382759`).
- `origin/main` reflog shows the last four updates were `update by push`, so `origin/main` only advances when the user pushes — not on local commits.
- No `worktree`/`cf496693` string in `~/.claude/settings.json`, `~/git/ducklink/.git/config`, `~/.gitconfig`, or `.claude/` hooks. There is no user-side pinning. This is Claude Code's built-in behavior.
- Each spawn also writes `.git/worktrees/agent-*/CLAUDE_BASE` recording the base commit it used — five of six currently show `cf496693…`.

## 3. Recurrence risk

**High, and silent.** Any agent spawned with `isolation: "worktree"` while local `main` is ahead of `origin/main` starts on a stale tree, sees files as "missing", and can produce reports that contradict the parent's known state. Phase 3's agent noticed and fast-forwarded; Phases 2d and 5 didn't. The gap widens with every unpushed merge.

## 4. Prevention (do not apply)

Any one of:

1. **Push `main` before spawning worktree agents** — cheapest, keeps `origin/main == main`.
2. **Parent fast-forwards the worktree** at spawn start (Phase 3 did this manually) — brief instruction in the spawn prompt.
3. **File a Claude Code request** to base worktrees on the local checked-out branch tip instead of `origin/<default>`, or to expose the base ref.
