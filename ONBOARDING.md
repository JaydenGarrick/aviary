# Onboarding — aviary 🐦

A Ratatui cockpit for a flock of repo-resident Claude Code agents. Each **bird**
is a real `claude` session living in its own repo with its own persistent
context and character; aviary spawns, names, displays, and connects them.

## 10-minute tour

```
 you ──┬── sidebar (roster)            one claude session PER REPO
       │     birds + rooms             named aviary-<id>, resumed by name
       │     status chips + unread     ┌─────────────────────────────┐
       └── content pane ───────────────│ bird PTY  or  room transcript│
                                       └─────────────────────────────┘
 signals:  claude agents --json (3s poll)  +  Stop/Notification hooks
           (per-session via --settings → aviary --hook → events.jsonl)
 birds ↔ birds:  native SendMessage (session names) + handoff briefs
 rooms:  append-only markdown transcripts, mention-driven dispatch
```

1. **Build & install**: `cargo install --path .` (hooks embed the binary path —
   a stable install beats `target/`). Requires Claude Code ≥ 2.1.224.
2. **First run**: `aviary` scaffolds `~/.config/aviary/` (config, personas,
   MCP hooks, permissions template). `aviary doctor` checks everything.
3. **Fly a bird**: `⏎` on Swift → a full claude session boots in the iOS repo
   with its persona. `ctrl+a` hands the keyboard back. Answer the repo-trust
   dialog once per repo.
4. **Hand off**: `@` → the selected bird packages its context and SendMessages
   a teammate. Rooms (`c`) are shared transcripts; birds reply only when
   @-mentioned or materially useful.

## The three load-bearing ideas

- **Session name = identity.** `--name aviary-<id>` / `--resume aviary-<id>`.
  No session bookkeeping anywhere; Claude Code owns storage.
- **The filesystem is the IPC.** Rooms, handoffs, hook events, config, state —
  all files under `~/.config/aviary/`, tick-polled. Crash-proof, inspectable,
  no sockets (except the opt-in webhook listener).
- **Status is layered truth**: fresh PTY output → working; poll/hook
  observation → busy/done/**needs-input** (the state a terminal can't show);
  recency heuristic as fallback. Transitions drive unread dots + banners.

## Where things live

| Area | Files |
|---|---|
| Shell: layered key dispatch, msg fan-out | `src/app.rs` (keep it thin) |
| Screens (state + input + draw + hit rects) | `src/components/{roster,thread,room}.rs` |
| Birds: spawn/resume/status/collab | `src/agent_store.rs` |
| Embedded terminal (PTY + vt100) | `src/pty.rs` |
| Keymaps → dispatch + hints + help (one source) | `src/keymap.rs` |
| Pure, tested logic | `src/{room,prompts,routine,events,http}.rs` |
| Config/scaffold/state | `src/config.rs` · personas in `assets/birds/` |

Read `CLAUDE.md` for the hard rules before changing anything — the launch-argv
ordering, dispatch-layer order, and status precedence are all load-bearing and
test-guarded.

## Verify your changes

`cargo test` (45+ unit tests) · `cargo clippy --all-targets` must stay at zero
· `aviary doctor` green · then the manual smoke: wake a bird, block it on a
permission prompt, watch `⏸ needs you` + the banner arrive.

## Git & publishing

Personal repo (`github.com/JaydenGarrick/aviary`, private). Commits use the
repo-local noreply identity. To push: `gh auth switch --user JaydenGarrick`,
push, then switch back to the work account.
