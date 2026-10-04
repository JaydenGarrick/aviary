# aviary roadmap — research-backed (2026-10-04)

Sources: Claude Code official docs (hooks-guide, agent-view, channels, mcp,
auto-mode-config, scheduled-tasks, env-vars — verified by a research agent),
Mobbin captures of Grok Bot's bot-profile/routines/notifications screens, and
observed first-run friction in real use.

## Tier 1 — real signals (highest value, unblocks everything else)

1. **Authoritative status from `claude agents --json`.** The agent view lists
   sessions with `Needs input / Working / Completed` states and supports
   `attach <id>` / `stop <id>`. Poll it from the Command executor (~2s) and map
   by session name → replaces the output-recency heuristic and adds the state
   we cannot currently see: **⏸ needs you** (blocked on a permission prompt or
   question). Sidebar chips become truthful.
2. **Hook events pipeline.** `Stop` fires when a session finishes responding;
   `Notification` fires with matchers `permission_prompt` (~6s after a prompt
   stalls), `idle_prompt`, `agent_needs_input`, `elicitation_dialog`. Payload
   carries `session_id` + `cwd` — enough to attribute to a bird by repo path.
   Hook command appends one JSON line to `~/.config/aviary/events.jsonl`;
   aviary tick-reads it (fs-as-state, as everything else). Open question to
   test first: whether `--settings <file>` accepts `hooks` (docs confirm it
   accepts `permissions`/`autoMode`; hooks unconfirmed) — fallback is an
   opt-in snippet installed into `~/.claude/settings.json` by `aviary doctor`.
   Hooks can also emit `terminalSequence` OSC titles, and vt100 exposes
   `set_window_title` callbacks — an in-band alternative channel per PTY.
3. **Finish/attention notifications + unread dots.** Grok Bot's per-agent
   toggle verbatim: "Get notified when this agent finishes or needs input."
   macOS notification via `osascript` when a bird hits Done/Needs-input while
   aviary is unfocused or another bird is selected; accent unread dot on the
   sidebar row until viewed.

## Tier 2 — friction and parity

4. **`aviary doctor` + first-run smoothing.** Checks: claude ≥ 2.1.224, repos
   exist, MCP servers authorized. Good news already confirmed: **MCP OAuth is
   per-user (keychain)** — authorizing Linear/Figma once in ANY session covers
   every bird. No supported pre-acceptance for the workspace trust dialog —
   document it as a one-time per-repo step. Optional per-bird permissions
   allowlist shipped via `--settings` (confirmed supported) to cut prompt noise
   for read-only tools.
5. **Routines per bird** (Grok Bot's bot profile: "Routines · Add routine").
   Config block per bot `{schedule, prompt}` → generated launchd/cron entries
   running `claude -p --resume aviary-<id> "<prompt>"`; `/loop` covers
   in-session cadence; cloud `/schedule` is the always-on tier. Routine runs
   land in the bird's own named session, so the TUI shows the results.
6. **Bot profile view in-app.** Edit persona in `$EDITOR`, pick glyph/colour,
   per-bird notification toggle, "Reset bird" (= `N`) — mirrors Grok Bot's
   profile screen and Oura's "Reset Advisor".
7. **Background birds.** Routine/long jobs via `--bg`; surface them through the
   same `claude agents --json` polling; `claude attach` to adopt one into the
   pane.

## Tier 3 — reach

8. **Channels webhook receiver** (docs show the pattern: a custom MCP channel
   accepting POSTs) — CI failures wake raven, Slack mentions wake a bird; the
   TUI itself could publish events instead of typing into PTYs.
9. **Richer rooms/handoffs**: recent-handoffs feed on the empty pane, quoted
   replies, Grok-Bot lettered quick-replies parsed from bot messages.
10. **A mockingbird bird** — the parity harness as a fourth teammate
    (inventory/scorecard questions, port kickoffs from a room).

## Non-features (researched, rejected for now)

- No `CLAUDE_SESSION_ID` env var exists — don't build on it (use hook payload
  `session_id`/`cwd`).
- Claude Code does NOT set OSC titles by default — title-parsing only works if
  our own hooks emit `terminalSequence`.
- `--dangerously-skip-permissions` as friction fix — rejected; scoped
  `--settings` allowlists only.
