---
name: flock-worker
description: "Act as a worker that an aviary orchestrator bird started as a background session: execute one workstream plan or one companion task, stay inside your scope, write a report for the orchestrator, and message it back. Use only when a prompt tells you to load the flock-worker skill."
---

# Flock worker

"The user" is the owner of this machine. Get their name with
`git config user.name`, and use that name in text for other agents and docs.

An orchestrator bird started you as a Claude Code background session. It gave
you a session name, a role, a workstream, a plan, a report file, and its own
address (its session name with a `[ref]`, for example `aviary-swift [1bc97a]`).
You do one job and report it. The orchestrator decides what comes next. The user watches you in the aviary cockpit and can attach to your
session at any time.

## Start

1. Read the plan file from your first prompt. Read it completely.
2. Read the repo's `AGENTS.md` or `CLAUDE.md`, if they exist. Obey them.
3. Confirm that your working directory is the worktree in the plan:
   `git rev-parse --show-toplevel` and `git branch --show-current`.
4. If the plan is not clear, write a `needs-input` report (see below) and stop.
   Do not guess the goal.

## Your role

- **`impl`**: Execute the plan's steps in order. You own all file edits in this
  worktree. Commit your work on the workstream branch in small, logical
  commits, in the repo's commit style. If the orchestrator copied the plan
  into your worktree, commit it with your work.
- **`research`**: Answer the question in your prompt. Read code, docs, or the
  web. Do not edit files in the repo.
- **`review`**: Review the diff against the base branch and the plan. Run the
  plan's "Done when" checks. Do not edit files in the repo.
- **`pr`**: Prepare the pull request: title, description, and the CI status.
  Read review comments and summarise them. Edit files only if the orchestrator
  tells you that you own the files now.

## Rules

- Stay inside the plan. Do not do work that is in "Out of scope". If you find
  other work that is necessary, put it in your report.
- Do not edit the plan or the board. The orchestrator owns them.
- Do not merge, rebase onto another branch, push, or open a pull request. Do
  these only when the orchestrator's prompt tells you to, and says that the
  user approved it.
- Do not edit files outside your worktree.
- Do not start, prompt, or stop other sessions.
- If a tool or a command asks for approval, let it wait. The cockpit shows you
  as "needs you", and the user answers.

## Report

Write your report before you end each turn. Write it to the report file from
your first prompt. Make the directory if it does not exist. Replace the old
content each time.

The cockpit reads the `status:` line, so the file must start exactly like
this: the title line, a blank line, then the status line.

```markdown
# <worker-session-name> report

status: done | needs-input | failed | in-progress

## Summary
What you did, in three sentences or less.

## Changes
- Commits, with hash and subject. Files that you changed.

## Checks
- Each command that you ran, and its result.

## Questions
- Each decision that you need from the orchestrator or the user.

## Follow-ups
- Work that you found but did not do.
```

Use `done` only when all of the plan's "Done when" items pass. Use
`needs-input` when you cannot continue without a decision. Use `failed` when
you tried and could not complete a step. Use `in-progress` when you stop with
more steps to do.

After you write the report, send the orchestrator one line with the
`SendMessage` tool: `REPORT: <report-file-path> status=<status>`. Address it
exactly as your first prompt gives the orchestrator — the session name with
its `[ref]` — never the bare name: another session may share it. If the send
is still refused as ambiguous, run `ListAgents` and send to the row whose
`[ref]` matches; if none matches, say so in your report instead of guessing.
Then end your turn with that same line.
