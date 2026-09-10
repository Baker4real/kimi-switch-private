---
type: reference
title: Issue tracker
tags: [github, issues, private]
intent: Route GitHub issue and PR operations to Baker4real/kimi-switch-private.
documents:
  config_keys:
    - Baker4real/kimi-switch-private
---
# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues on **Baker4real/kimi-switch-private**. Use the `gh` CLI for all operations.

Always pass `-R Baker4real/kimi-switch-private`. Do not infer the repo from `git remote -v` or from `origin`: `origin` is public upstream `yuan2627/kimi-switch` and is fetch-only. Creating, labelling, or commenting on that upstream is out of scope.

## Conventions

- **Create an issue**: `gh issue create -R Baker4real/kimi-switch-private --title "..." --body "..."`. Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view -R Baker4real/kimi-switch-private <number> --comments`, filtering comments by `jq` and also fetching labels.
- **List issues**: `gh issue list -R Baker4real/kimi-switch-private --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment -R Baker4real/kimi-switch-private <number> --body "..."`
- **Apply / remove labels**: `gh issue edit -R Baker4real/kimi-switch-private <number> --add-label "..."` / `--remove-label "..."`
- **Close**: `gh issue close -R Baker4real/kimi-switch-private <number> --comment "..."`

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external PRs as feature requests; `/triage` reads this flag.)_

When set to `yes`, PRs run through the same labels and states as issues, using the `gh pr` equivalents with `-R Baker4real/kimi-switch-private`:

- **Read a PR**: `gh pr view -R Baker4real/kimi-switch-private <number> --comments` and `gh pr diff -R Baker4real/kimi-switch-private <number>` for the diff.
- **List external PRs for triage**: `gh pr list -R Baker4real/kimi-switch-private --state open --json number,title,body,labels,author,authorAssociation,comments` then keep only `authorAssociation` of `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, or `NONE` (drop `OWNER`/`MEMBER`/`COLLABORATOR`).
- **Comment / label / close**: `gh pr comment`, `gh pr edit --add-label`/`--remove-label`, `gh pr close`, each with `-R Baker4real/kimi-switch-private`.

GitHub shares one number space across issues and PRs, so a bare `#42` may be either: resolve with `gh pr view -R Baker4real/kimi-switch-private 42` and fall back to `gh issue view -R Baker4real/kimi-switch-private 42`.

## When a skill says "publish to the issue tracker"

Create a GitHub issue on `Baker4real/kimi-switch-private`.

## When a skill says publish tickets (`/to-tickets`)

One GitHub issue per ticket. Do not close or rewrite the parent spec.

- Labels: one category (`enhancement` or `bug`) plus `ready-for-agent`, unless instructed otherwise.
- **Blocking**: same native `blocked_by` as Wayfinding below. `issue_id` is the blocker's numeric **database id**, not `#number`.
- **Parent/child**: attach each ticket as a GitHub sub-issue of the parent: `POST repos/Baker4real/kimi-switch-private/issues/<parent-number>/sub_issues` with JSON body `{"sub_issue_id": <child-database-id>}`. `sub_issue_id` must be a JSON integer. `gh api -F` stringifies the field and GitHub returns 422; write a UTF-8 JSON file and pass `--input`.
- GitHub issue database ids exceed .NET `Int32` (example ~5.4e9). Do not `[int]`-cast `.id`. Keep the decimal in the JSON text, or use `[long]`.
- Verify: parent `sub_issues_summary.total`, and each child's `issue_dependencies_summary`. `GET .../issues/<n>/sub_issues` may EOF; the parent summary is the live count.

## When a skill says "fetch the relevant ticket"

Run `gh issue view -R Baker4real/kimi-switch-private <number> --comments`.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single issue with **child** issues as tickets.

- **Map**: a single issue labelled `wayfinder:map`, holding the Notes / Decisions-so-far / Fog body. `gh issue create -R Baker4real/kimi-switch-private --label wayfinder:map`.
- **Child ticket**: an issue linked to the map as a GitHub sub-issue (`gh api` on the sub-issues endpoint). Where sub-issues aren't enabled, add the child to a task list in the map body and put `Part of #<map>` at the top of the child body. Labels: `wayfinder:<type>` (`research`/`prototype`/`grilling`/`task`). Once claimed, the ticket is assigned to the driving dev.
- **Blocking**: GitHub's **native issue dependencies**, the canonical, UI-visible representation. Add an edge with `gh api --method POST repos/Baker4real/kimi-switch-private/issues/<child>/dependencies/blocked_by -F issue_id=<blocker-db-id>`, where `<blocker-db-id>` is the blocker's numeric **database id** (`gh api repos/Baker4real/kimi-switch-private/issues/<n> --jq .id`, _not_ the `#number` or `node_id`). GitHub reports `issue_dependencies_summary.blocked_by` (open blockers only, the live gate). Where dependencies aren't available, fall back to a `Blocked by: #<n>, #<n>` line at the top of the child body. A ticket is unblocked when every blocker is closed.
- **Frontier query**: list the map's open children (`gh issue list -R Baker4real/kimi-switch-private --state open`, scoped to the map's sub-issues / task list), drop any with an open blocker (`issue_dependencies_summary.blocked_by > 0`, or an open issue in the `Blocked by` line) or an assignee; first in map order wins.
- **Claim**: `gh issue edit -R Baker4real/kimi-switch-private <n> --add-assignee @me`, the session's first write.
- **Resolve**: `gh issue comment -R Baker4real/kimi-switch-private <n> --body "<answer>"`, then `gh issue close -R Baker4real/kimi-switch-private <n>`, then append a context pointer (gist + link) to the map's Decisions-so-far.
