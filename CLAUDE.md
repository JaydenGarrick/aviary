# CLAUDE.md — aviary

A Ratatui cockpit for repo-resident Claude Code agents ("birds"). Component
architecture throughout — read this map before touching the shell.

## Architecture map

| Module | Owns |
|---|---|
| `app.rs` | the THIN shell: layered key dispatch, overlay routing, msg fan-out, run loop. If a change grows this file, it probably belongs in a component. |
| `components/` | one module per screen (roster · thread · room), each owning state + input + draw + its own hit rects. Bots and rooms are STATE inside components, never new components. |
| `shared.rs` | cross-component state (`Shared`): config, `AgentStore`, room watcher, branch slot, focus. |
| `agent_store.rs` | the birds: spawn/resume (`--name aviary-<id>[.n] --session-id <uuid>` / `--resume <uuid>`), stop, status, keyed by `SessionKey` (bird × tab; tab 1 = primary), each PTY running one CONVERSATION (`ConvKey`: a tab's own, or tab 1's home / a room's). Routes external signals (`signal` → `route`) and owns the ONE place tab 1 switches conversation (`pump_switches` → `next_switch`). No jsonl bookkeeping. Owns the PTYs and the status LAYERING (`pick_status`); the signals beyond the PTY live in its `ModLayer`. Types the dead-mod inbox fallback itself (`sweep_inbox`) and derives the chip text components draw (`detail` · `reporting_key` · `attention_text`). Per-key observations are dropped in ONE place (`forget_where`). |
| `mod_layer.rs` | `ModLayer`, owned by `AgentStore` (as `Workers` is): per-key session ids, poll kind + `waitingFor`, the hooks' last needs-input, the mod's status records, and the inbox (`delivery` · `post` · `take_for_typing` · `inbox_move` on a new id · `gc`). Bounded fs only. |
| `status.rs` | the status vocabulary (`StatusKind` · `BotStatus` · `Detail` · `map_status_str`), pure — its own module so the store, flock, status files and UI depend on it, not on each other. |
| `pty.rs` | embedded terminal (portable-pty + vt100 + tui-term). Resize is guarded; wheel forwards SGR on the alt screen. |
| `keymap.rs` | binding tables — dispatch, the hints bar, and the help overlay all generate from them. Never hand-write a hint string. |
| `command.rs` | `Executor` (thread-per-command) + `Slot<T>` for async results with stale-gen dropping. |
| `room.rs` / `prompts.rs` | pure logic: transcript parse/append/dispatch, and the exact text typed into birds. Both unit-tested — change the tests with the wording. |
| `overlays.rs` | modal forms (new bird, new room, compose, bird profile), shell-owned. |
| `markdown.rs` | pure: a message body + width → styled lines (tables with column squeeze + stacked fallback, lists, fences, quotes, inline bold/code/@mentions). Unit-tested — change the tests with the layout. |
| `clipboard.rs` | `pbcopy` — the only clipboard path. |
| `events.rs` | the classic hook pipeline — the STABLE FLOOR of status: birds run `aviary --hook` on Stop · Notification · PermissionRequest · StopFailure (per-session via `--settings`); events land in events.jsonl, read offset-tracked each tick. |
| `status_file.rs` | the MOD's files: `status/<sessionId>.json` (the mod inside every bird and worker writes it on each change + a 5 s heartbeat; read on change each tick, joined by the poll's `sessionId`) and `inbox/<sessionId>/*.md` (prompts for RUNNING sessions; the mod submits them with `$.prompt.submit` and acks). A thin fs adapter around pure parse/decide logic — `ModStatus::reconcile` is the one place the poll may speak over a live mod; `PostStamp` keeps inbox names monotonic. Tempdir-tested. The mod itself is `assets/plugin/hooks/register.ts` (TypeScript, runs inside claude; `claude plugin validate --strict assets/plugin` + `claude plugin test assets/plugin`). |
| `routine.rs` | schedule grammar + due-math (pure, tested); the shell fires due routines every 30s. |
| `flock.rs` | the birds' WORKERS: `claude --bg` sessions a bird spawns itself, attributed from the poll by name (`aviary-<bird>_<slug>-<role>`), status from poll + the mod's status file + `reports/<name>.md`. Pure, unit-tested; owned by `AgentStore::workers`. Aviary never spawns one. |
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
  handoffs, routines, and webhooks all funnel through `Shared::signal`, each
  naming a CONVERSATION: rooms their own, everything else home. Extra session
  tabs (`aviary-<id>.<n>`) are human-driven; routing signals at them
  reintroduces unbounded fan-out. The `.` tab separator is safe because
  `slug()` can never emit one into a bot id.
- **Session identity is the PINNED id, not the name.** Every fresh launch is
  `--name aviary-<id>[.n] --session-id <uuid>` with a uuid aviary minted
  (`State::begin_fresh`, never a reused one — claude refuses "already in
  use"); every later one is `--resume <uuid>`. The name only ADDRESSES the
  bird (SendMessage, hooks) and is shared by every conversation tab 1 ever
  ran — claude refuses `--resume <name>` once two sessions share it. Records
  live in state.json `convs` (`ConvRecord { sid, created }`); a v0.3.1
  `spawned` entry migrates to `{sid: None}` and resumes by name once. The
  poll joins rows by pinned id first (`poll_matches`; the name fallback takes
  only a unique row no other record owns), hooks by their `session_id` (a
  late hook from a conversation no longer running is dropped), and gc keeps
  every recorded id's queue. A launch SEEDS its id into `ModLayer` and
  clears the slot's retired one (`ModLayer::seed`) — or `note_poll` would
  move one conversation's queue into another.
- **Prompts into FRESH sessions ride argv**, never typed into a booting PTY.
  A session already RUNNING gets its prompt through ONE ordered queue, the
  inbox (`inbox/<sessionId>/`) — known at launch for a pinned session
  (`AgentStore::delivery` → `Delivery::Inbox`). A live mod submits each file
  with `$.prompt.submit` (a turn of its own, once idle) and acks it; for a
  mod not heard from for 30 s (or never — claude < 2.1.287) the shell types
  the oldest file, one per tick, never into a PTY younger than MARK_AFTER.
  Immediate keystrokes (`send_line`) remain only for a v0.3.1 session the
  poll has not named yet, or when the inbox write fails. A resume that dies
  on arrival relaunches fresh WITH the prompt that rode it.
- **Each room owns a conversation per member** (`ConvKey` `<bird>#<room>`),
  run in tab 1 — the bird's HOME conversation is `<bird>`. Room creation
  touches no session and never eager-spawns; the room's first dispatch to a
  bird hatches its room conversation with the prompt on argv. Tab 1 runs ONE
  conversation (`active` in state.json): a signal for the running one is
  delivered, a stopped tab 1 launches the target, and a live tab 1 on
  ANOTHER conversation is never relaunched by a signal — the prompt queues in
  the target's inbox (`AgentStore::signal` → `route`). `pump_switches` is the
  ONE place a live tab 1 changes conversation, behind pure `next_switch`:
  Done ≥ 5 s, booted ≥ MARK_AFTER, its own queue drained, no keystroke from
  the person for 30 s, nothing typed by aviary for 5 s, no workers out
  (their reports address the conversation that spawned them). Then: an
  outright want (the person's tab-menu pick — pinned until a turn runs in
  it — a handoff's wake, a deleted room sending it home), else the OLDEST
  queued prompt, which rides argv; the rest drain through the mod. Pending
  switches are files, so they survive a restart. Deleting a room forgets its
  conversations and queues.
- **Mouse gestures have an owner.** The pane that takes `Down` receives
  `Drag`/`Up` until release (`App::drag_owner`), so a drag-select in the room
  finishes even when the button comes up over the sidebar. Selection lives
  in CONTENT coordinates (body line, display column) and copies on release.
- **Launch argv order is load-bearing:** `--mcp-config` and `--add-dir` are
  VARIADIC claude flags — each must be followed by another flag or it swallows
  the positional prompt as another value. `launch_args()` owns the order and
  a unit test guards it; never append args after the prompt or reorder it.
- **Status precedence is layered and ordered:** the mod's status file while
  its heartbeat is fresh (<15 s, `ModStatus::is_alive`) → its state (it saw
  the turn start, the dialog open, the turn die — nothing below may
  contradict it, and the poll/hook must not `observe` a key the mod is alive
  for, or the kind flaps). TWO exceptions, both in ONE pure function,
  `ModStatus::reconcile` (status_file.rs), worn by birds and workers alike:
  (a) no engine event marks a permission dialog answered — a poll taken
  ≥ `POLL_LAG` (3 s) after the mod said `needs-input · permission` that reads
  busy with no `waitingFor` reads the mod as `working`; (b) the mod sees only
  permission/question/plan dialogs — a fresh poll `waitingFor` on a session
  the mod calls quiet/working reads as `needs-input` with that reason.
  Else: fresh PTY output (<2s) → Working; recent poll/hook
  observation (<15s) → its kind; else the output-recency heuristic
  (`pick_status`, pure, tested). `map_status_str` maps unknown poll strings
  to Done and `ModStatus::status` maps unknown mod states to None — never
  invent urgency from an unrecognized state. Workers: the poll's exit wins,
  then a live mod, then the poll, then the report refining idle.
- **Worker name grammar is a hard rule:** `aviary-<bird>_<slug>-<role>` —
  `_` is the worker separator, `.` the tab separator, `#` the room one (state
  keys only); `slug()` emits none and config load rejects ids containing
  them, so `SessionKey::parse_session_name`, `ConvKey::parse_record_key` and
  `flock::WorkerName::parse` are provably disjoint (test-guarded). Roles:
  impl · research · review · pr. A worker only attributes to a CONFIGURED bird.
- **Workers are poll-attributed, never spawned by aviary.** The bird spawns
  them (the shipped `flock-orchestrator` skill); aviary watches `claude agents
  --json`, reads `reports/<name>.md`, and offers `attach`/`stop`. A worker
  row leaves after 3 missed polls — a failed poll delivers an EMPTY list, so
  one must never wipe the flock. Viewer tabs (`claude attach <id>`,
  `SessionKind::Attached`) never earn a resume record and closing one never
  stops the worker. Worker status: the poll's exit wins; a live mod (via
  `ModStatus::reconcile`) owns the running states, else the poll; the report
  refines idle (done · needs-input · failed · in-progress).
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
- **The plugin has two ownerships.** `plugin/skills/*` are prose: never
  overwritten. The manifest and the MOD (`plugin/hooks/*`, `plugin/types/*`,
  `MOD_FILES`) are aviary-owned: rewritten whenever the shipped text differs
  (`write_owned`), never touched when equal — birds hot-reload the folder on
  every save, and the mod's status-file schema (`v: 1`) must match the binary
  reading it. The engine lays `plugin/.claude-plugin/types/` beside a loaded
  plugin; it is not ours and nothing reads or writes it. The mod finds the
  config dir through `AVIARY_CONFIG_DIR`, set on every bird's PTY (workers
  inherit it). The mod ships unconditionally; `aviary doctor` warns below
  2.1.287, where it simply does not load and the classic hooks are the
  whole truth.

## Verify

`cargo test` (125 unit tests: keymap, list_nav, config scaffold + mod
materialization, user-name resolution, session keys, status layering, hook
attribution, room dispatch, prompt wording, markdown layout, room selection
math, worker name grammar + report precedence, status-file
parse/scan/inbox/gc) · `cargo clippy --all-targets` must be clean · `claude
plugin validate --strict assets/plugin` and `claude plugin test
assets/plugin` for the mod · manual smoke: see README keys. Claude Code ≥
2.1.224 required at runtime; ≥ 2.1.287 for the mod; ≥ 2.1.289 for flock
workers (`--bg` · `attach` · `stop` · `--plugin-dir`).
