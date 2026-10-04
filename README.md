# aviary 🐦

A terminal cockpit for a flock of repo-resident Claude Code agents — inspired by
Grok Bot's "agents as contacts" model. Each **bird** is a real `claude` session
living in its own repo with its own persistent context and character; aviary is
the roster that spawns, names, displays, and connects them.

```
┌─────────────────────────────────────────────────────────┐
│ AVIARY                               ● 2 flying · 3 birds│
│                                                         │
│   BIRDS                                                 │
│ ▸ 🪶 swift    ios       ● working      ⎇ develop        │
│        “Hand off to @aviary-raven: …”                   │
│   🧺 weaver   android   · idle 4m      ⎇ main           │
│   🐦‍⬛ raven    core-api  ○ not started                   │
│                                                         │
│   ROOMS                                                 │
│   # fly-calculator   @swift · @raven                    │
│                                                         │
│ j move · ⏎ open · a talk · @ message · ? keys           │
└─────────────────────────────────────────────────────────┘
```

## Why

Working across sibling repos means several agents with isolated contexts: the
iOS agent knows everything about a feature, and there is no good way to hand
that context to the backend or Android agent. Aviary gives each repo a bird and
gives the birds each other.

## How a bird works

- **One session per repo, resumed by name.** A bird spawns as
  `claude --name aviary-<id>` in its repo; every later open is
  `claude --resume aviary-<id>` — the conversation (and the repo context in it)
  persists across aviary restarts.
- **Character via `--append-system-prompt-file`.** Each bird has a persona file
  in `~/.config/aviary/birds/` — identity and voice, teammates, room protocol.
  The repo's own CLAUDE.md stays the law; the persona is who the bird *is*.
- **Birds message each other natively.** Claude Code's cross-session messaging
  (`ListAgents` / `SendMessage`) works between the birds' sessions; personas
  teach them to hand work across repo boundaries by session name, writing
  larger packages to `~/.config/aviary/handoffs/` first.
- **`@` is the handoff key.** From a bird's thread, `@` asks *that bird* to
  package its context (files, endpoints, constraints) and send it to a
  teammate. From the roster, `@` messages a bird directly.
- **Rooms are shared transcripts.** A room is an append-only markdown file
  under `~/.config/aviary/rooms/`; birds append `### @name — time` blocks.
  Dispatch is mention-driven: your message with no mentions wakes every member,
  a bird's reply only wakes who it @-mentions — so bot chatter is bounded.
- **Linear + Figma come along.** Every bird spawns with
  `--mcp-config ~/.config/aviary/mcp.json` (Linear + Figma remote MCP servers),
  and composers detect `ABC-123` / linear.app / figma.com references and wrap
  them in fetch instructions.

## Quickstart

```bash
cargo install --path .     # or: cargo run
aviary
```

First run scaffolds `~/.config/aviary/` with three default birds (edit
`config.json` for your own repos): **swift** 🪶 (iOS), **weaver** 🧺 (Android),
**raven** 🐦‍⬛ (core-api). `n` hatches more.

Requires Claude Code ≥ 2.1.224 (cross-session messaging + `--name`).

## Keys

| Key | Where | Does |
|---|---|---|
| `j/k` `⏎` | roster | move · open a thread or room |
| `a` | roster/thread | wake the bird and take the keyboard |
| `ctrl+a` | anywhere | hand the keyboard back to aviary |
| `@` | roster/thread | message a bird / hand off via the current bird |
| `n` / `g` | anywhere | new bird / new room |
| `x` | roster/thread | stop a session (it resumes by name later) |
| `⏎` | room | write to the room |
| `?` | anywhere | help, generated from the real keymaps |

## Layout

Everything stateful lives outside this repo, in `~/.config/aviary/`:
`config.json` (bots + rooms) · `birds/*.md` (personas) · `mcp.json` (hooks) ·
`rooms/*.md` (transcripts) · `handoffs/` (briefs) · `state.json`.
