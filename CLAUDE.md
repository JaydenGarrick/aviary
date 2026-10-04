# CLAUDE.md — aviary

A Ratatui cockpit for repo-resident Claude Code agents ("birds"). Component
architecture throughout — read this map before touching the shell.

## Architecture map

| Module | Owns |
|---|---|
| `app.rs` | the THIN shell: layered key dispatch, overlay routing, msg fan-out, run loop. If a change grows this file, it probably belongs in a component. |
| `components/` | one module per screen (roster · thread · room), each owning state + input + draw + its own hit rects. Bots and rooms are STATE inside components, never new components. |
| `shared.rs` | cross-component state (`Shared`): config, `AgentStore`, room watcher, branch slot, focus. |
| `agent_store.rs` | the birds: spawn/resume (`--name` / `--resume aviary-<id>`), stop, status. Session identity IS the session name — no jsonl bookkeeping. |
| `pty.rs` | embedded terminal (portable-pty + vt100 + tui-term). Resize is guarded; wheel forwards SGR on the alt screen. |
| `keymap.rs` | binding tables — dispatch, the hints bar, and the help overlay all generate from them. Never hand-write a hint string. |
| `command.rs` | `Executor` (thread-per-command) + `Slot<T>` for async results with stale-gen dropping. |
| `room.rs` / `prompts.rs` | pure logic: transcript parse/append/dispatch, and the exact text typed into birds. Both unit-tested — change the tests with the wording. |
| `overlays.rs` | modal forms (new bird, new room, compose, bird profile), shell-owned. |
| `events.rs` | the hook pipeline: birds run `aviary --hook` on Stop/Notification (per-session via `--settings`); events land in events.jsonl, read offset-tracked each tick. |
| `routine.rs` | schedule grammar + due-math (pure, tested); the shell fires due routines every 30s. |
| `http.rs` | inbound webhooks (POST /bird/<id> · /room/<id>, bearer-gated, localhost, off unless configured). |
| `doctor.rs` | `aviary doctor` — environment ✓/✗ screen. |

## Hard rules

- **No tokio.** Plain threads over one `mpsc`; the run loop coalesces events.
- **Subprocess spawns never run on the UI thread** — `Command` + `Executor`.
  Bounded local-fs work (<~1 ms) may stay inline.
- **Key dispatch layers (ordered, load-bearing):** ① PTY-focused (ctrl+a
  escapes) · ② help · ③ interrupts (ctrl+c — ABOVE capturing on purpose) ·
  ④ capturing forms/fields · ⑤ component keymap · ⑥ global keymap.
- **fs-as-state:** rooms, handoffs, config, and spawn-state are files under
  `~/.config/aviary/` (override: `AVIARY_CONFIG_DIR`). Nothing stateful in this
  repo; no sockets, no IPC.
- **Room dispatch is mention-driven** — a bot append wakes only who it
  @-mentions. Loosening this reintroduces unbounded bot chatter; don't.
- **Prompts into FRESH sessions ride argv**, never typed into a booting PTY.
  Typed input is only for sessions already running.
- **Launch argv order is load-bearing:** `--mcp-config` and `--add-dir` are
  VARIADIC claude flags — each must be followed by another flag or it swallows
  the positional prompt as another value. `launch_args()` owns the order and
  a unit test guards it; never append args after the prompt or reorder it.
- **Status precedence is layered and ordered:** fresh PTY output (<2s) →
  Working; recent poll/hook observation (<15s) → its kind; else the
  output-recency heuristic. `map_status_str` maps unknown poll strings to Done
  — never invent urgency from an unrecognized state.
- **Default files materialize on every startup** (`materialize_defaults`),
  per-file and never overwriting — new shipped personas/templates reach
  existing installs. Only `scaffold()` (first run) writes config.json.

## Verify

`cargo test` (26+ unit tests: keymap, list_nav, config scaffold, room
dispatch, prompt wording) · `cargo clippy --all-targets` must be clean ·
manual smoke: see README keys. Claude Code ≥ 2.1.224 required at runtime.
