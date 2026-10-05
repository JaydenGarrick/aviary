# aviary 🐦

A terminal cockpit for a flock of repo-resident Claude Code agents — inspired by
Grok Bot's "agents as contacts" model. Each **bird** is a real `claude` session
living in its own repo with its own persistent context and character; aviary is
the roster that spawns, names, displays, and connects them.

```
┌──────────────────────────────────────────────────────────────────┐
│ AVIARY                                        ● 2 flying · 3 birds│
│ BIRDS             │ 🪶 Swift · ~/…/ios · ● working · ⎇ develop   │
│▸🪶 Swift ● working │ ┌──────────────────────────────────────────┐ │
│   ↗ @raven “…”    │ │                                          │ │
│ 🧺 Weaver ✔ done 4m│ │   the selected bird's live claude        │ │
│ 🐦‍⬛ Raven ● working │ │   session (or the room transcript),      │ │
│   ↘ @swift        │ │   always on the right — selection         │ │
│ ROOMS             │ │   switches it instantly                   │ │
│ # fly-calc    2   │ │                                          │ │
│   @swift @raven   │ └──────────────────────────────────────────┘ │
│ + bird  + room    │                                              │
│ j move · ⏎ open · a talk · @ handoff · ? keys                    │
└──────────────────────────────────────────────────────────────────┘
```

The sidebar tells you what every bird is doing at a glance — and the signals
are REAL, not guessed: each bird's session runs Stop/Notification hooks (wired
per-session via `--settings`) and aviary reconciles against
`claude agents --json` every 3s:

- `● working` · `✔ done 3m` (finished, waiting) · **`⏸ needs you`** (blocked on
  a permission prompt or question — the state a terminal can't show you) ·
  `○ not started` · `✗ exited`
- an accent `●` unread dot when a bird finished or got blocked while you were
  looking elsewhere (clears when you select it)
- a macOS banner when a bird **finishes or needs input** (per-bird toggle in
  its profile, `p`)
- a **collaboration tag**: `↗ @raven` (handed work off), `↘ @swift` (received a
  handoff), `⇄ #room` (room work). No tag = flying solo; clears when you take
  the keyboard.

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

## Keys (everything is mouse-clickable too)

| Key | Does |
|---|---|
| `j/k` / click | select a bird or room — the content pane follows instantly |
| `⏎` / `a` / click pane | step in: wake the bird and take the keyboard, or write the room |
| `ctrl+a` / `esc` | hand the keyboard back to the sidebar |
| `@` | handoff: the selected bird packages its context for a teammate |
| `n` / `c` (or the `+ bird` / `+ room` buttons) | hatch a bird / create a room |
| `p` | bird profile: persona, routines, notifications toggle, fresh start |
| `N` | abandon the bird's conversation and start a fresh one |
| `x` | stop a bird (all its sessions; they resume by name later) |
| `T` / click `[+]` | open a parallel session of the bird in a new tab |
| `]` / `[` / click a tab | cycle / pick the bird's session tabs |
| `W` | close the viewed tab and forget it (tab 1 refuses — that's the bird) |
| `i` / `u` / `d` / `1-5` | write / scroll the room / pick a quick-reply |
| `?` | help, generated from the real keymaps |

Mouse: click rows to select (again to step in), click chips/fields/members in
any form, click outside a popup to dismiss it, wheel scrolls everything —
including the bird's own transcript inside its pane.

## Doctor, routines, webhooks

- **`aviary doctor`** — one screen of ✓/✗: claude version, repos, config, MCP
  hooks, the events pipeline, plus the mimus 🪞 (mockingbird parity oracle)
  config snippet. Run it whenever something feels off.
- **Routines** — per-bird scheduled prompts in `config.json`
  (`{"id":"standup","schedule":"weekdays@09:00","prompt":"…"}`; grammar:
  `daily@HH:MM` · `weekdays@HH:MM` · `every:<N>m|h`). Fire while aviary runs,
  into the bird's own session; listed in its profile (`p`).
- **Webhooks** — add `"webhook": {"port": 4242, "token": "…"}` to config.json
  and external events wake birds:
  `curl -H "Authorization: Bearer …" -d "CI red on main" \
  localhost:4242/bird/raven` (also `/room/<id>` — the message dispatches to
  members like any outside author).
- **Permissions** — set a bird's `"permissions": "permissions/readonly.json"`
  to pre-allow read-only tools and cut prompt noise (template scaffolded).

## Layout

Everything stateful lives outside this repo, in `~/.config/aviary/`:
`config.json` (bots + rooms + webhook) · `birds/*.md` (personas) · `mcp.json`
(MCP hooks) · `settings/*.json` (per-bird hook settings, regenerated at
launch) · `events.jsonl` (hook events) · `rooms/*.md` (transcripts) ·
`handoffs/` (briefs) · `permissions/` (allowlists) · `state.json`.
