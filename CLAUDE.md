# CLAUDE.md — aviary

A Ratatui cockpit for repo-resident Claude Code agents ("birds"). Component
architecture throughout — read this map before touching the shell.

## Architecture map

| Module | Owns |
|---|---|
| `app.rs` | the THIN shell: layered key dispatch, overlay routing, msg fan-out, run loop. If a change grows this file, it probably belongs in a component. |
| `components/` | one module per screen (roster · thread · room), each owning state + input + draw + its own hit rects. Bots and rooms are STATE inside components, never new components. |
| `shared.rs` | cross-component state (`Shared`): config, `AgentStore`, room watcher, branch slot, focus. |
| `agent_store.rs` | the birds: spawn/resume (`--name` / `--resume aviary-<id>[.n]`), stop, status, keyed by `SessionKey` (bird × tab; tab 1 = primary). Session identity IS the session name — no jsonl bookkeeping. |
| `pty.rs` | embedded terminal (portable-pty + vt100 + tui-term). Resize is guarded; wheel forwards SGR on the alt screen. |
| `keymap.rs` | binding tables — dispatch, the hints bar, and the help overlay all generate from them. Never hand-write a hint string. |
| `command.rs` | `Executor` (thread-per-command) + `Slot<T>` for async results with stale-gen dropping. |
| `room.rs` / `prompts.rs` | pure logic: transcript parse/append/dispatch, and the exact text typed into birds. Both unit-tested — change the tests with the wording. |
| `overlays.rs` | modal forms (new bird, new room, compose, bird profile), shell-owned. |
| `markdown.rs` | pure: a message body + width → styled lines (tables with column squeeze + stacked fallback, lists, fences, quotes, inline bold/code/@mentions). Unit-tested — change the tests with the layout. |
| `clipboard.rs` | `pbcopy` — the only clipboard path. |
| `events.rs` | the hook pipeline: birds run `aviary --hook` on Stop/Notification (per-session via `--settings`); events land in events.jsonl, read offset-tracked each tick. |
| `routine.rs` | schedule grammar + due-math (pure, tested); the shell fires due routines every 30s. |
| `flock.rs` | the birds' WORKERS: `claude --bg` sessions a bird spawns itself, attributed from the poll by name (`aviary-<bird>_<slug>-<role>`), status from poll + `reports/<name>.md`. Pure, unit-tested; owned by `AgentStore::workers`. Aviary never spawns one. |
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
- **External signals target the PRIMARY session (tab 1) only** — rooms,
  handoffs, routines, and webhooks all funnel through `Shared::boot_bot`.
  Extra session tabs (`aviary-<id>.<n>`) are human-driven; routing signals at
  them reintroduces unbounded fan-out. The `.` tab separator is safe because
  `slug()` can never emit one into a bot id.
- **Prompts into FRESH sessions ride argv**, never typed into a booting PTY.
  Typed input is only for sessions already running.
- **Room creation RESETS members' primary sessions without relaunching** —
  drop the PTY, forget the resume record (`AgentStore::reset_primary`); the
  room's first dispatch hatches them fresh with the prompt on argv. Never
  eager-spawn at creation: the composer opens immediately and the first
  message would be keystrokes at a booting PTY. Tabs are untouched.
- **Mouse gestures have an owner.** The pane that takes `Down` receives
  `Drag`/`Up` until release (`App::drag_owner`), so a drag-select in the room
  finishes even when the button comes up over the sidebar. Selection lives
  in CONTENT coordinates (body line, display column) and copies on release.
- **Launch argv order is load-bearing:** `--mcp-config` and `--add-dir` are
  VARIADIC claude flags — each must be followed by another flag or it swallows
  the positional prompt as another value. `launch_args()` owns the order and
  a unit test guards it; never append args after the prompt or reorder it.
- **Status precedence is layered and ordered:** fresh PTY output (<2s) →
  Working; recent poll/hook observation (<15s) → its kind; else the
  output-recency heuristic. `map_status_str` maps unknown poll strings to Done
  — never invent urgency from an unrecognized state.
- **Worker name grammar is a hard rule:** `aviary-<bird>_<slug>-<role>` —
  `_` is the worker separator, `.` the tab separator; `slug()` emits neither
  and config load rejects ids containing them, so `SessionKey::parse_session_name`
  and `flock::WorkerName::parse` are provably disjoint (test-guarded). Roles:
  impl · research · review · pr. A worker only attributes to a CONFIGURED bird.
- **Workers are poll-attributed, never spawned by aviary.** The bird spawns
  them (the shipped `flock-orchestrator` skill); aviary watches `claude agents
  --json`, reads `reports/<name>.md`, and offers `attach`/`stop`. A worker
  row leaves after 3 missed polls — a failed poll delivers an EMPTY list, so
  one must never wipe the flock. Viewer tabs (`claude attach <id>`,
  `SessionKind::Attached`) never earn a resume record and closing one never
  stops the worker. Worker status: poll owns the live states (busy / needs
  input / exited); the report refines idle (done · needs-input · failed ·
  in-progress).
- **Aviary ships no birds.** `scaffold()` (first run) writes an EMPTY
  config.json; birds are hatched from the cockpit (`n`) or by hand. Shipped
  support files (mcp.json, permissions/readonly.json) materialize on every
  startup (`materialize_defaults`), per-file and never overwriting, so new
  ones reach existing installs. The empty roster is a real state — the pane
  and hints bar draw a zero state for it, with keys pulled via
  `keymap::label_for`. The flock skills ship the same way, as a plugin under
  `plugin/` (`--plugin-dir` on every bird's argv; the orchestrator skill
  passes it to workers) — `aviary doctor` notes a copy that differs from the
  shipped text.

## Verify

`cargo test` (97 unit tests: keymap, list_nav, config scaffold, user-name resolution, session keys,
status aggregation, hook attribution, room dispatch, prompt wording, markdown
layout, room selection math, worker name grammar + report precedence) ·
`cargo clippy --all-targets` must be clean · manual smoke: see README keys.
Claude Code ≥ 2.1.224 required at runtime; ≥ 2.1.289 for flock workers
(`--bg` · `attach` · `stop` · `--plugin-dir`).
