---
name: onboarding
description: Walk a new contributor (or a fresh Claude session) through the aviary codebase — what birds are, how signals flow, where the load-bearing rules live, and how to verify changes. Use when someone new opens this repo, asks how aviary works, or wants a guided tour before making changes.
---

# Aviary onboarding tour

You are orienting someone in the aviary repo. Give them a working mental model
fast, grounded in the real files — never from memory alone.

## Steps

1. **Read `ONBOARDING.md` and `CLAUDE.md`** (repo root). These are the map and
   the law. Summarize the three load-bearing ideas in your own words:
   session-name-as-identity, filesystem-as-IPC, layered status truth.
2. **Show the shape.** Walk `src/` top-down in this order, one sentence each:
   `main.rs` (modes: TUI, `--hook`, `doctor`) → `app.rs` (the thin shell + the
   SIX dispatch layers, in order) → `components/` (roster owns the keys;
   thread/room are the content pane) → `agent_store.rs` (spawn/resume/status) →
   the pure modules (`room`, `prompts`, `routine`, `events`, `http`) — point
   out that each pure module carries its own unit tests.
3. **Name the landmines** (these have regressed before; all are test-guarded):
   - `launch_args()` ordering — `--mcp-config`/`--add-dir` are VARIADIC and
     will eat the positional prompt.
   - Interrupts (ctrl+c) sit ABOVE capturing in the dispatch layers.
   - Fresh sessions are only marked resumable after surviving 10s.
   - `map_status_str` maps unknown states to Done — never invent urgency.
4. **Run the proof**: `cargo test` and `cargo clippy --all-targets` (must be
   zero warnings), then `aviary doctor` if a config exists on this machine.
5. **Close with the contributor loop**: pick a seam (new Action → keymap table
   → component `update()`; new background work → `Command` + `Executor`; new
   modal → `overlays.rs`), write the change test-first where a pure module is
   involved, keep `app.rs` thin, and update CLAUDE.md if a new rule is born.

Tailor depth to the request: a quick question gets the mental model (step 1 +
relevant landmine); "give me the tour" gets all five steps.
