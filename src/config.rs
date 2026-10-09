//! Config, personas, rooms, and state — all under one directory.
//!
//! `~/.config/aviary/` (override: `AVIARY_CONFIG_DIR`):
//!   config.json   bots + rooms (user- AND app-edited; pretty-printed)
//!   birds/*.md    persona files, one per hatched bird (from the template), user-editable
//!   mcp.json      MCP servers every bird spawns with (Linear + Figma hooks)
//!   rooms/*.md    group-chat transcripts (append-only)
//!   handoffs/     where birds park large handoff briefs
//!   plugin/       the flock skills, loaded by birds and workers via --plugin-dir
//!   reports/      worker report files (`<worker-name>.md` with a `status:` line)
//!   state.json    which bots have ever spawned (drives --resume vs fresh)
//!
//! Everything lives OUTSIDE the aviary repo on purpose: bot configs name
//! work-repo paths and handoff content concerns work code — none of which
//! belongs in a personal GitHub repo.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct BotId(pub String);

impl fmt::Display for BotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl BotId {
    /// The Claude Code session name — prefixed so bots are stable @mention
    /// targets that do not collide with the user's own named sessions.
    pub fn session_name(&self) -> String {
        format!("aviary-{}", self.0)
    }
}

/// One live session of one bird: the primary (tab 1, every external signal's
/// target) or an extra human-driven tab. Tab 1 keeps the bird's historical
/// names so existing resume records, handoff targets, and mentions stay valid.
/// The `.` separator cannot collide with a bot id — [`slug`] never emits one.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SessionKey {
    pub bot: BotId,
    /// 1-based; 1 is the primary.
    pub tab: u8,
}

impl SessionKey {
    pub fn primary(bot: BotId) -> SessionKey {
        SessionKey { bot, tab: 1 }
    }

    /// `aviary-<id>` for the primary, `aviary-<id>.<n>` for tabs.
    pub fn session_name(&self) -> String {
        match self.tab {
            1 => self.bot.session_name(),
            n => format!("{}.{n}", self.bot.session_name()),
        }
    }

    /// The `state.json` spawned-set entry: `<id>` or `<id>.<n>`.
    pub fn state_key(&self) -> String {
        match self.tab {
            1 => self.bot.0.clone(),
            n => format!("{}.{n}", self.bot.0),
        }
    }

    /// Inverse of [`SessionKey::session_name`].
    pub fn parse_session_name(name: &str) -> Option<SessionKey> {
        Self::parse_state_key(name.strip_prefix("aviary-")?)
    }

    /// Inverse of [`SessionKey::state_key`].
    pub fn parse_state_key(key: &str) -> Option<SessionKey> {
        // `_` is the worker separator (`flock::WorkerName`): a worker name is
        // never a bird or a tab. `slug()` emits neither separator.
        if key.is_empty() || key.contains(crate::flock::SEP) {
            return None;
        }
        match key.split_once('.') {
            None => Some(SessionKey::primary(BotId(key.to_string()))),
            Some((id, n)) => {
                let tab: u8 = n.parse().ok()?;
                (tab >= 2 && !id.is_empty()).then(|| SessionKey {
                    bot: BotId(id.to_string()),
                    tab,
                })
            }
        }
    }
}

fn default_true() -> bool {
    true
}

/// A scheduled prompt for one bird. Grammar: `daily@HH:MM`, `weekdays@HH:MM`,
/// `every:<N>m|h` — parsed in `routine.rs`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub schedule: String,
    pub prompt: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Bot {
    pub id: BotId,
    pub name: String,
    pub glyph: String,
    /// As written in config (may start with `~`); expand via [`Bot::repo_path`].
    pub repo: String,
    /// Persona file, relative to the config dir (or absolute).
    pub persona: String,
    /// macOS banners when this bird finishes or needs input.
    #[serde(default = "default_true")]
    pub notify: bool,
    /// Optional permission-allowlist settings JSON (relative to the config
    /// dir), merged into the bird's per-session `--settings` file.
    #[serde(default)]
    pub permissions: Option<String>,
    #[serde(default)]
    pub routines: Vec<Routine>,
}

impl Bot {
    pub fn repo_path(&self) -> PathBuf {
        expand_tilde(&self.repo)
    }

    pub fn persona_path(&self, dir: &Path) -> PathBuf {
        let p = expand_tilde(&self.persona);
        if p.is_absolute() {
            p
        } else {
            dir.join(p)
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Room {
    pub id: String,
    pub name: String,
    pub members: Vec<BotId>,
}

impl Room {
    pub fn transcript_path(&self, dir: &Path) -> PathBuf {
        dir.join("rooms").join(format!("{}.md", self.id))
    }
}

/// Inbound webhooks (`POST /bird/<id>` · `/room/<id>`) — absent = off.
#[derive(Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    pub port: u16,
    pub token: String,
}

#[derive(Default, Serialize, Deserialize)]
struct RawConfig {
    /// The human's room author name; absent = derived from the login env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_name: Option<String>,
    #[serde(default)]
    bots: Vec<Bot>,
    #[serde(default)]
    rooms: Vec<Room>,
    #[serde(default)]
    webhook: Option<WebhookConfig>,
}

pub struct Config {
    pub dir: PathBuf,
    /// The human's room author name, resolved — see [`resolve_user_name`].
    pub user_name: String,
    /// `user_name` exactly as config.json has it (None = not set), so
    /// [`Config::save`] round-trips it and never pins an env-derived name.
    pub user_name_cfg: Option<String>,
    pub bots: Vec<Bot>,
    pub rooms: Vec<Room>,
    pub webhook: Option<WebhookConfig>,
}

/// Where the config lives: `AVIARY_CONFIG_DIR`, else `~/.config/aviary`.
pub fn default_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("AVIARY_CONFIG_DIR") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join(".config").join("aviary"))
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(s)
}

/// What the human posts as when nothing names them.
const DEFAULT_USER_NAME: &str = "you";

/// The human's room author name and where it came from: config.json
/// `user_name` → `$USER` → `$LOGNAME` → "you". Values are trimmed and an
/// empty one counts as absent. Pure, so the chain is tested without env.
pub fn resolve_user_name(
    configured: Option<&str>,
    env_user: Option<&str>,
    env_logname: Option<&str>,
) -> (String, &'static str) {
    let candidates = [
        (configured, "config.json"),
        (env_user, "$USER"),
        (env_logname, "$LOGNAME"),
    ];
    candidates
        .into_iter()
        .find_map(|(value, source)| {
            let v = value?.trim();
            (!v.is_empty()).then(|| (v.to_string(), source))
        })
        .unwrap_or_else(|| (DEFAULT_USER_NAME.to_string(), "default"))
}

/// [`resolve_user_name`] against the real environment. Env vars only —
/// never `whoami`; a subprocess is not worth a name.
pub fn user_name_from_env(configured: Option<&str>) -> (String, &'static str) {
    let user = std::env::var("USER").ok();
    let logname = std::env::var("LOGNAME").ok();
    resolve_user_name(configured, user.as_deref(), logname.as_deref())
}

impl Config {
    /// Load the config, scaffolding the directory on first run (an EMPTY
    /// roster — aviary ships no birds). Existing installs still get
    /// newly-shipped support files (mcp.json, permissions) materialized —
    /// per-file, never overwriting edits.
    pub fn load_or_scaffold(dir: PathBuf) -> Result<Config> {
        let config_path = dir.join("config.json");
        materialize_defaults(&dir)?;
        if !config_path.is_file() {
            scaffold(&dir)?;
        }
        let text = std::fs::read_to_string(&config_path)
            .with_context(|| format!("cannot read {}", config_path.display()))?;
        let raw: RawConfig = serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", config_path.display()))?;

        let mut seen = BTreeSet::new();
        for b in &raw.bots {
            if !seen.insert(b.id.clone()) {
                bail!("config.json declares bot id {:?} twice", b.id.0);
            }
            if !valid_bot_id(&b.id.0) {
                bail!(
                    "config.json bot id {:?} may not contain '.' or '_' — they separate session tabs and worker names",
                    b.id.0
                );
            }
        }

        // The human counts as an OUTSIDE author in room dispatch only while
        // no bird shares the name. It is never a session name, so `.`/`_`
        // are fine here — a login like `jayden.garrick` must keep working.
        let (user_name, source) = user_name_from_env(raw.user_name.as_deref());
        if user_name.chars().any(char::is_whitespace) {
            bail!(
                "user_name {user_name:?} (from {source}) may not contain whitespace — set \"user_name\" in config.json"
            );
        }
        let shares_name = |b: &Bot| b.id.0.eq_ignore_ascii_case(&user_name);
        if raw.bots.iter().any(shares_name) {
            bail!(
                "user_name {user_name:?} (from {source}) collides with a bot id — the human and a bird cannot share a name; set \"user_name\" in config.json"
            );
        }

        Ok(Config {
            dir,
            user_name,
            user_name_cfg: raw.user_name,
            bots: raw.bots,
            rooms: raw.rooms,
            webhook: raw.webhook,
        })
    }

    pub fn bot(&self, id: &BotId) -> Option<&Bot> {
        self.bots.iter().find(|b| &b.id == id)
    }

    pub fn room(&self, id: &str) -> Option<&Room> {
        self.rooms.iter().find(|r| r.id == id)
    }

    pub fn mcp_config_path(&self) -> PathBuf {
        self.dir.join("mcp.json")
    }

    pub fn handoffs_dir(&self) -> PathBuf {
        self.dir.join("handoffs")
    }

    fn save(&self) -> Result<()> {
        let raw = RawConfig {
            user_name: self.user_name_cfg.clone(),
            bots: self.bots.clone(),
            rooms: self.rooms.clone(),
            webhook: self.webhook.clone(),
        };
        let json = serde_json::to_string_pretty(&raw)?;
        std::fs::write(self.dir.join("config.json"), json + "\n")?;
        Ok(())
    }

    /// Remove a bird from the roster. Its persona file and claude session
    /// survive (resumable with `claude --resume aviary-<id>`); it also leaves
    /// every room, and a room left with fewer than two birds dissolves too.
    /// Returns the ids of rooms that dissolved.
    pub fn remove_bot(&mut self, id: &BotId) -> Result<Vec<String>> {
        if self.bot(id).is_none() {
            bail!("no bot named {:?}", id.0);
        }
        self.bots.retain(|b| &b.id != id);
        let mut dissolved = Vec::new();
        for room in &mut self.rooms {
            room.members.retain(|m| m != id);
        }
        self.rooms.retain(|r| {
            if r.members.len() < 2 {
                dissolved.push(r.id.clone());
                false
            } else {
                true
            }
        });
        self.save()?;
        Ok(dissolved)
    }

    /// Remove a room from the roster. The transcript file stays on disk.
    pub fn remove_room(&mut self, id: &str) -> Result<()> {
        if self.room(id).is_none() {
            bail!("no room named {id:?}");
        }
        self.rooms.retain(|r| r.id != id);
        self.save()
    }

    /// Flip a bird's notification toggle and persist it.
    pub fn set_notify(&mut self, id: &BotId, on: bool) -> Result<()> {
        if let Some(bot) = self.bots.iter_mut().find(|b| &b.id == id) {
            bot.notify = on;
            self.save()?;
        }
        Ok(())
    }

    /// The per-session `--settings` file: Stop + Notification hooks (the
    /// truthful done/needs-input signals, probed working via `--settings`),
    /// merged with the bird's optional permission allowlist. Regenerated every
    /// launch — the hook command embeds the current exe and the session name
    /// (`--session`), which is what lets hook events name their tab; cwd alone
    /// cannot, since every tab shares the repo.
    pub fn settings_file_for(&self, bot: &Bot, key: &SessionKey) -> Result<PathBuf> {
        let dir = self.dir.join("settings");
        std::fs::create_dir_all(&dir)?;
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "aviary".into());
        let hook = serde_json::json!([{ "hooks": [{
            "type": "command",
            "command": format!("\"{exe}\" --hook --session {}", key.session_name()),
        }]}]);
        let mut root = serde_json::json!({
            "hooks": { "Stop": hook.clone(), "Notification": hook }
        });

        if let Some(rel) = &bot.permissions {
            let p = expand_tilde(rel);
            let path = if p.is_absolute() { p } else { self.dir.join(p) };
            let extra: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?,
            )
            .with_context(|| format!("{} is not valid JSON", path.display()))?;
            if let (Some(root_map), Some(extra_map)) = (root.as_object_mut(), extra.as_object()) {
                for (k, v) in extra_map {
                    root_map.insert(k.clone(), v.clone());
                }
            }
        }

        let path = dir.join(format!("{}.json", key.state_key()));
        std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")?;
        Ok(path)
    }

    /// Where `aviary --hook` appends events and the shell reads them.
    pub fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    /// The flock plugin (skills) every bird loads via `--plugin-dir`, and
    /// that the orchestrator skill passes on to workers.
    pub fn plugin_dir(&self) -> PathBuf {
        self.dir.join("plugin")
    }

    /// Where workers drop `<worker-name>.md` reports (read each tick).
    pub fn reports_dir(&self) -> PathBuf {
        self.dir.join("reports")
    }

    /// Add a bot: validate, materialize a persona from the template, persist.
    pub fn add_bot(&mut self, id: &str, name: &str, glyph: &str, repo: &str) -> Result<BotId> {
        let id = slug(id);
        if id.is_empty() {
            bail!("a bot needs an id (letters, digits, dashes)");
        }
        let bot_id = BotId(id.clone());
        if self.bot(&bot_id).is_some() {
            bail!("a bot named {id:?} already exists");
        }
        let repo_path = expand_tilde(repo);
        if !repo_path.is_dir() {
            bail!("{} is not a directory", repo_path.display());
        }

        let persona_rel = format!("birds/{id}.md");
        let persona_abs = self.dir.join(&persona_rel);
        if !persona_abs.is_file() {
            let body = PERSONA_TEMPLATE
                .replace("{{ID}}", &id)
                .replace("{{NAME}}", name)
                .replace("{{GLYPH}}", glyph)
                .replace("{{REPO}}", repo);
            std::fs::write(&persona_abs, body)?;
        }

        self.bots.push(make_bot(
            bot_id.clone(),
            name,
            glyph,
            repo,
            &persona_rel,
        ));
        self.save()?;
        Ok(bot_id)
    }

    /// Create a room: validate members, touch the transcript, persist.
    pub fn add_room(&mut self, name: &str, members: Vec<BotId>) -> Result<String> {
        let id = slug(name);
        if id.is_empty() {
            bail!("a room needs a name");
        }
        if self.room(&id).is_some() {
            bail!("a room named {id:?} already exists");
        }
        if members.len() < 2 {
            bail!("a room needs at least two birds — for one, open its thread");
        }
        for m in &members {
            if self.bot(m).is_none() {
                bail!("no bot named {:?}", m.0);
            }
        }
        let room = Room {
            id: id.clone(),
            name: name.trim().to_string(),
            members,
        };
        let path = room.transcript_path(&self.dir);
        if !path.is_file() {
            let roster: Vec<String> = room.members.iter().map(|m| format!("@{m}")).collect();
            std::fs::write(
                &path,
                format!("# #{} — {}\n", room.id, roster.join(" · ")),
            )?;
        }
        self.rooms.push(room);
        self.save()?;
        Ok(id)
    }
}

/// A bot id may not carry either session-name separator: `.` starts a tab
/// suffix and `_` starts a worker suffix. `slug()` never emits them; this
/// guards hand-edited config.json.
pub fn valid_bot_id(s: &str) -> bool {
    !s.is_empty() && !s.contains('.') && !s.contains(crate::flock::SEP)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

// ------------------------------------------------------------------ scaffold

const PERSONA_TEMPLATE: &str = include_str!("../assets/birds/template.md");
/// The flock skills, shipped into `<dir>/plugin/skills/` on startup.
pub(crate) const ORCHESTRATOR_SKILL: &str = include_str!("../assets/skills/flock-orchestrator/SKILL.md");
pub(crate) const WORKER_SKILL: &str = include_str!("../assets/skills/flock-worker/SKILL.md");

fn make_bot(id: BotId, name: &str, glyph: &str, repo: &str, persona: &str) -> Bot {
    Bot {
        id,
        name: name.to_string(),
        glyph: glyph.to_string(),
        repo: repo.to_string(),
        persona: persona.to_string(),
        notify: true,
        permissions: None,
        routines: Vec::new(),
    }
}

/// Directory tree + every shipped support file, written only when missing —
/// safe to run on every startup, so upgrades deliver new files without ever
/// overwriting a user's edits. No personas ship: `birds/` fills as the user
/// hatches birds (see [`Config::add_bot`]).
fn materialize_defaults(dir: &Path) -> Result<()> {
    for sub in ["birds", "rooms", "handoffs", "reports"] {
        std::fs::create_dir_all(dir.join(sub))
            .with_context(|| format!("cannot create {}", dir.join(sub).display()))?;
    }

    // Opt-in read-only permission allowlist a bot can reference via its
    // `permissions` field (served to claude through `--settings`).
    let perms_dir = dir.join("permissions");
    std::fs::create_dir_all(&perms_dir)?;
    let readonly = perms_dir.join("readonly.json");
    if !readonly.is_file() {
        std::fs::write(
            &readonly,
            serde_json::to_string_pretty(&serde_json::json!({
                "permissions": { "allow": [
                    "Read", "Grep", "Glob",
                    "Bash(git status:*)", "Bash(git log:*)", "Bash(git diff:*)"
                ]}
            }))? + "\n",
        )?;
    }

    // The Linear + Figma hooks: every bird spawns with `--mcp-config` pointing
    // here, so ticket and design tools exist regardless of repo-level config.
    // Auth happens once per server through each session's normal MCP flow.
    let mcp = dir.join("mcp.json");
    if !mcp.is_file() {
        std::fs::write(
            &mcp,
            serde_json::to_string_pretty(&serde_json::json!({
                "mcpServers": {
                    "linear": { "type": "sse", "url": "https://mcp.linear.app/sse" },
                    "figma":  { "type": "http", "url": "https://mcp.figma.com/mcp" }
                }
            }))? + "\n",
        )?;
    }

    // The flock skills ride `--plugin-dir`: every bird loads this plugin for
    // its session, and the orchestrator skill passes it to the workers it
    // spawns. Per-file and never overwriting, like mcp.json — a user's edits
    // survive upgrades (`aviary doctor` notes when a copy differs).
    let plugin = dir.join("plugin");
    let manifest_dir = plugin.join(".claude-plugin");
    std::fs::create_dir_all(&manifest_dir)?;
    let manifest = manifest_dir.join("plugin.json");
    if !manifest.is_file() {
        std::fs::write(
            &manifest,
            serde_json::to_string_pretty(&serde_json::json!({
                "name": "aviary",
                "description": "aviary flock skills — orchestrate background worker sessions and report back",
                "version": env!("CARGO_PKG_VERSION"),
                "author": { "name": "aviary" },
            }))? + "\n",
        )?;
    }
    for (name, text) in SHIPPED_SKILLS {
        let skill_dir = plugin.join("skills").join(name);
        std::fs::create_dir_all(&skill_dir)?;
        let skill = skill_dir.join("SKILL.md");
        if !skill.is_file() {
            std::fs::write(&skill, text)?;
        }
    }

    Ok(())
}

/// `(skill name, shipped text)` — the plugin's `skills/<name>/SKILL.md` files.
pub(crate) const SHIPPED_SKILLS: [(&str, &str); 2] = [
    ("flock-orchestrator", ORCHESTRATOR_SKILL),
    ("flock-worker", WORKER_SKILL),
];

/// First run only: an empty roster. Birds are hatched from the cockpit (`n`)
/// or by editing config.json — nothing repo-specific ships.
fn scaffold(dir: &Path) -> Result<()> {
    let default_config = RawConfig {
        user_name: None,
        bots: Vec::new(),
        rooms: Vec::new(),
        webhook: None,
    };
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_string_pretty(&default_config)? + "\n",
    )?;
    Ok(())
}

// --------------------------------------------------------------------- state

/// Which bots have ever had a session — decides `--resume` vs a fresh spawn —
/// plus when each routine last fired.
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    spawned: BTreeSet<String>,
    /// `"<bot>/<routine id>"` → unix seconds of the last firing.
    #[serde(default)]
    routine_runs: std::collections::BTreeMap<String, u64>,
    /// State key → a human label for the tab strip. Display only — the
    /// SESSION name stays `aviary-<id>[.n]`, or resume would break.
    #[serde(default)]
    tab_names: std::collections::BTreeMap<String, String>,
}

impl State {
    pub fn load(dir: &Path) -> State {
        std::fs::read_to_string(dir.join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn spawned_once(&self, state_key: &str) -> bool {
        self.spawned.contains(state_key)
    }

    pub fn mark_spawned(&mut self, dir: &Path, state_key: &str) {
        self.spawned.insert(state_key.to_string());
        self.write(dir);
    }

    /// Forget a session of record so the next launch starts fresh (a dead
    /// resume, a closed tab, a deliberate fresh start). The label survives —
    /// a fresh conversation on a named tab keeps its name; `set_tab_name`
    /// with an empty string clears it where that is wanted.
    pub fn forget(&mut self, dir: &Path, state_key: &str) {
        self.spawned.remove(state_key);
        self.write(dir);
    }

    pub fn tab_name(&self, state_key: &str) -> Option<&str> {
        self.tab_names.get(state_key).map(String::as_str)
    }

    /// Label a tab for the strip; an empty name clears the label.
    pub fn set_tab_name(&mut self, dir: &Path, state_key: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.tab_names.remove(state_key);
        } else {
            self.tab_names.insert(state_key.to_string(), name.to_string());
        }
        self.write(dir);
    }

    /// Every tab of this bird with a session of record (`<id>` → 1,
    /// `<id>.<n>` → n). Order is ascending by tab.
    pub fn spawned_tabs(&self, id: &BotId) -> Vec<u8> {
        let mut tabs: Vec<u8> = self
            .spawned
            .iter()
            .filter_map(|k| SessionKey::parse_state_key(k))
            .filter(|k| k.bot == *id)
            .map(|k| k.tab)
            .collect();
        tabs.sort_unstable();
        tabs
    }

    /// Drop every session record of one bird — primary and tabs alike.
    pub fn forget_bot(&mut self, dir: &Path, id: &BotId) {
        let foreign = |k: &str| SessionKey::parse_state_key(k).is_none_or(|key| key.bot != *id);
        self.spawned.retain(|k| foreign(k));
        self.tab_names.retain(|k, _| foreign(k));
        self.write(dir);
    }

    pub fn routine_last_run(&self, bot: &BotId, routine_id: &str) -> Option<u64> {
        self.routine_runs.get(&format!("{bot}/{routine_id}")).copied()
    }

    pub fn mark_routine_run(&mut self, dir: &Path, bot: &BotId, routine_id: &str, epoch: u64) {
        self.routine_runs.insert(format!("{bot}/{routine_id}"), epoch);
        self.write(dir);
    }

    fn write(&self, dir: &Path) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(dir.join("state.json"), json + "\n");
        }
    }
}

/// Test fixture: a scaffolded config dir with birds `ids` hatched into
/// throwaway repos under it. Aviary ships no birds, so tests seed their own.
#[cfg(test)]
pub fn test_flock(dir: &Path, ids: &[&str]) -> Config {
    let mut cfg = Config::load_or_scaffold(dir.to_path_buf()).unwrap();
    for id in ids {
        let repo = dir.join("repos").join(id);
        std::fs::create_dir_all(&repo).unwrap();
        cfg.add_bot(id, id, "🐦", repo.to_str().unwrap()).unwrap();
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(ids: &[&str]) -> (tempfile::TempDir, Config) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_flock(tmp.path(), ids);
        (tmp, cfg)
    }

    #[test]
    fn scaffold_creates_an_empty_roster_and_hooks() {
        let (tmp, cfg) = temp_config(&[]);
        assert!(cfg.bots.is_empty(), "aviary ships no birds");
        assert!(cfg.rooms.is_empty());
        assert!(tmp.path().join("config.json").is_file());
        // No shipped personas — birds/ exists for the ones the user hatches.
        assert_eq!(std::fs::read_dir(tmp.path().join("birds")).unwrap().count(), 0);
        assert!(tmp.path().join("mcp.json").is_file());
        assert!(tmp.path().join("permissions/readonly.json").is_file());
        assert!(tmp.path().join("rooms").is_dir());
        assert!(tmp.path().join("handoffs").is_dir());
        // The flock: a plugin (manifest + both skills) and the reports dir.
        assert!(tmp.path().join("reports").is_dir());
        assert_eq!(cfg.reports_dir(), tmp.path().join("reports"));
        assert_eq!(cfg.plugin_dir(), tmp.path().join("plugin"));
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("plugin/.claude-plugin/plugin.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["name"], "aviary");
        for (name, text) in SHIPPED_SKILLS {
            let path = tmp.path().join("plugin/skills").join(name).join("SKILL.md");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "{name} ships verbatim");
            assert!(text.starts_with(&format!("---\nname: {name}\n")), "{name} frontmatter");
        }
        // Reload reads what scaffold wrote.
        let again = Config::load_or_scaffold(tmp.path().to_path_buf()).unwrap();
        assert!(again.bots.is_empty());
    }

    #[test]
    fn materialize_never_overwrites_a_customized_skill() {
        let (tmp, _cfg) = temp_config(&[]);
        let skill = tmp.path().join("plugin/skills/flock-worker/SKILL.md");
        std::fs::write(&skill, "---\nname: flock-worker\n---\nmy edit\n").unwrap();
        std::fs::remove_file(tmp.path().join("plugin/skills/flock-orchestrator/SKILL.md")).unwrap();
        Config::load_or_scaffold(tmp.path().to_path_buf()).unwrap();
        assert_eq!(std::fs::read_to_string(&skill).unwrap(), "---\nname: flock-worker\n---\nmy edit\n");
        // A missing file comes back — upgrades deliver new skills, never clobber edits.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("plugin/skills/flock-orchestrator/SKILL.md")).unwrap(),
            ORCHESTRATOR_SKILL
        );
    }

    #[test]
    fn config_rejects_ids_with_tab_or_worker_separators() {
        for (id, sep) in [("night_jar", '_'), ("a.b", '.')] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("config.json"),
                format!(
                    r#"{{"bots":[{{"id":"{id}","name":"x","glyph":"🐦","repo":"/tmp","persona":"birds/x.md"}}],"rooms":[]}}"#
                ),
            )
            .unwrap();
            let err = Config::load_or_scaffold(tmp.path().to_path_buf())
                .err()
                .expect("a reserved separator in an id must fail the load");
            assert!(err.to_string().contains(&format!("{id:?}")), "{sep}: {err:#}");
            assert!(!valid_bot_id(id));
        }
        assert!(valid_bot_id("night-jar"));
        assert!(!valid_bot_id(""));
    }

    #[test]
    fn user_name_resolves_config_then_env_then_default() {
        // config.json wins over the environment.
        assert_eq!(
            resolve_user_name(Some("finch"), Some("envuser"), Some("logname")),
            ("finch".to_string(), "config.json")
        );
        // Absent → $USER → $LOGNAME → "you"; blanks count as absent.
        assert_eq!(
            resolve_user_name(None, Some("envuser"), Some("logname")),
            ("envuser".to_string(), "$USER")
        );
        assert_eq!(
            resolve_user_name(Some("  "), Some(""), Some(" logname ")),
            ("logname".to_string(), "$LOGNAME")
        );
        assert_eq!(
            resolve_user_name(None, None, None),
            ("you".to_string(), "default")
        );
        // Trimmed; a dotted login name is fine — it is never a session name.
        assert_eq!(
            resolve_user_name(Some(" jayden.garrick "), None, None).0,
            "jayden.garrick"
        );
    }

    #[test]
    fn configured_user_name_wins_over_env_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"user_name":"finch","bots":[],"rooms":[]}"#,
        )
        .unwrap();
        let mut cfg = Config::load_or_scaffold(tmp.path().to_path_buf()).unwrap();
        assert_eq!(cfg.user_name, "finch", "config.json beats $USER/$LOGNAME");
        assert_eq!(cfg.user_name_cfg.as_deref(), Some("finch"));
        // A save keeps what was configured.
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        cfg.add_bot("swift", "Swift", "🐦", repo.to_str().unwrap()).unwrap();
        let text = std::fs::read_to_string(tmp.path().join("config.json")).unwrap();
        assert!(text.contains(r#""user_name": "finch""#), "{text}");
    }

    #[test]
    fn save_never_writes_an_unconfigured_user_name() {
        // An env-derived name stays out of config.json.
        let (tmp, mut cfg) = temp_config(&[]);
        assert!(cfg.user_name_cfg.is_none());
        assert!(!cfg.user_name.is_empty(), "something always resolves");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        cfg.add_bot("swift", "Swift", "🐦", repo.to_str().unwrap()).unwrap();
        let text = std::fs::read_to_string(tmp.path().join("config.json")).unwrap();
        assert!(!text.contains("user_name"), "{text}");
    }

    #[test]
    fn user_name_rejects_bot_id_collisions_and_whitespace() {
        // A bird and the human cannot share a name — the human would stop
        // counting as an outside author in room dispatch.
        for (name, needle) in [
            ("swift", "collides with a bot id"),
            ("Swift", "collides with a bot id"),
            ("jay den", "whitespace"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("config.json"),
                format!(
                    r#"{{"user_name":"{name}","bots":[{{"id":"swift","name":"x","glyph":"🐦","repo":"/tmp","persona":"birds/x.md"}}],"rooms":[]}}"#
                ),
            )
            .unwrap();
            let err = Config::load_or_scaffold(tmp.path().to_path_buf())
                .err()
                .expect("must fail the load");
            assert!(err.to_string().contains(needle), "{name}: {err:#}");
        }
    }

    #[test]
    fn add_bot_materializes_persona_and_persists() {
        let (tmp, mut cfg) = temp_config(&[]);
        let repo = tmp.path().join("some-repo");
        std::fs::create_dir(&repo).unwrap();
        let id = cfg
            .add_bot("Night Jar", "Nightjar", "🌙", repo.to_str().unwrap())
            .unwrap();
        assert_eq!(id.0, "night-jar");
        assert_eq!(id.session_name(), "aviary-night-jar");
        let persona = std::fs::read_to_string(tmp.path().join("birds/night-jar.md")).unwrap();
        assert!(persona.contains("aviary-night-jar"));
        assert!(!persona.contains("{{ID}}"));
        let again = Config::load_or_scaffold(tmp.path().to_path_buf()).unwrap();
        assert_eq!(again.bots.len(), 1);
    }

    #[test]
    fn add_bot_rejects_duplicates_and_bad_repos() {
        let (tmp, mut cfg) = temp_config(&["swift"]);
        assert!(cfg
            .add_bot("swift", "Swift", "x", tmp.path().to_str().unwrap())
            .is_err());
        assert!(cfg.add_bot("newbie", "N", "x", "/definitely/not/a/dir").is_err());
    }

    #[test]
    fn add_room_touches_transcript_and_validates_members() {
        let (tmp, mut cfg) = temp_config(&["swift", "raven"]);
        let id = cfg
            .add_room(
                "Fly Calculator",
                vec![BotId("swift".into()), BotId("raven".into())],
            )
            .unwrap();
        assert_eq!(id, "fly-calculator");
        let path = tmp.path().join("rooms/fly-calculator.md");
        assert!(path.is_file());
        assert!(std::fs::read_to_string(path).unwrap().contains("@swift"));
        // one member → refused; unknown member → refused
        assert!(cfg.add_room("solo", vec![BotId("swift".into())]).is_err());
        assert!(cfg
            .add_room("ghosts", vec![BotId("swift".into()), BotId("nope".into())])
            .is_err());
    }

    #[test]
    fn state_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = State::default();
        assert!(!st.spawned_once("swift"));
        st.mark_spawned(tmp.path(), "swift");
        assert!(State::load(tmp.path()).spawned_once("swift"));
        st.forget(tmp.path(), "swift");
        assert!(!State::load(tmp.path()).spawned_once("swift"));

        // Tab entries share the set; forget_bot sweeps a bird's whole family
        // without touching a bird whose id merely shares the prefix.
        st.mark_spawned(tmp.path(), "swift");
        st.mark_spawned(tmp.path(), "swift.2");
        st.mark_spawned(tmp.path(), "swiftly");
        let loaded = State::load(tmp.path());
        assert!(loaded.spawned_once("swift.2"));
        assert_eq!(loaded.spawned_tabs(&BotId("swift".into())), vec![1, 2]);
        st.forget_bot(tmp.path(), &BotId("swift".into()));
        let loaded = State::load(tmp.path());
        assert!(!loaded.spawned_once("swift"));
        assert!(!loaded.spawned_once("swift.2"));
        assert!(loaded.spawned_once("swiftly"));
    }

    #[test]
    fn tab_names_persist_and_clean_up() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = State::default();
        st.set_tab_name(tmp.path(), "swift.2", "refactor");
        st.set_tab_name(tmp.path(), "swift", "main");
        st.set_tab_name(tmp.path(), "raven.3", "triage");
        assert_eq!(State::load(tmp.path()).tab_name("swift.2"), Some("refactor"));
        // Empty and whitespace names clear.
        st.set_tab_name(tmp.path(), "swift", "  ");
        assert_eq!(st.tab_name("swift"), None);
        // A fresh start forgets the record but KEEPS the label.
        st.forget(tmp.path(), "swift.2");
        assert_eq!(st.tab_name("swift.2"), Some("refactor"));
        // Releasing a bird forgets every label.
        st.forget_bot(tmp.path(), &BotId("raven".into()));
        assert_eq!(State::load(tmp.path()).tab_name("raven.3"), None);
    }

    #[test]
    fn session_key_names_and_state_keys() {
        let primary = SessionKey::primary(BotId("swift".into()));
        assert_eq!(primary.session_name(), "aviary-swift");
        assert_eq!(primary.state_key(), "swift");
        let tab = SessionKey { bot: BotId("night-jar".into()), tab: 3 };
        assert_eq!(tab.session_name(), "aviary-night-jar.3");
        assert_eq!(tab.state_key(), "night-jar.3");

        assert_eq!(SessionKey::parse_session_name("aviary-swift"), Some(primary));
        assert_eq!(SessionKey::parse_session_name("aviary-night-jar.3"), Some(tab.clone()));
        assert_eq!(SessionKey::parse_state_key("night-jar.3"), Some(tab));
        // Not ours / malformed: no prefix, tab 1 spelled out, junk suffix.
        assert_eq!(SessionKey::parse_session_name("swift"), None);
        assert_eq!(SessionKey::parse_state_key("swift.1"), None);
        assert_eq!(SessionKey::parse_state_key("swift.x"), None);
        assert_eq!(SessionKey::parse_state_key(".2"), None);
        // A worker name is never a bird or a tab (see flock::WorkerName).
        assert_eq!(SessionKey::parse_state_key("swift_x-impl"), None);
        assert_eq!(SessionKey::parse_session_name("aviary-swift_x-impl"), None);
        assert_eq!(SessionKey::parse_session_name("aviary-swift_x-impl.2"), None);
    }

    #[test]
    fn slug_never_emits_the_tab_separator() {
        // The `.` in `<id>.<n>` is only unambiguous because no bot id can
        // contain one — a bird named "night.jar" slugs the dot away.
        assert_eq!(slug("night.jar"), "night-jar");
        assert_eq!(slug("v2.0 bird"), "v2-0-bird");
        assert!(!slug("a.b.c-2.9").contains('.'));
        // Same for the worker separator: `aviary-<bird>_<slug>-<role>`.
        assert_eq!(slug("night_jar"), "night-jar");
        assert!(!slug("a_b__c").contains(crate::flock::SEP));
        assert!(valid_bot_id(&slug("Night_Jar.2")));
    }

    #[test]
    fn settings_file_embeds_session_flag() {
        let (tmp, cfg) = temp_config(&["swift"]);
        let bot = cfg.bots[0].clone();
        let key = SessionKey { bot: bot.id.clone(), tab: 2 };
        let path = cfg.settings_file_for(&bot, &key).unwrap();
        assert!(path.ends_with("settings/swift.2.json"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("--hook --session aviary-swift.2"));
        // The primary keeps its historical filename.
        let primary = cfg
            .settings_file_for(&bot, &SessionKey::primary(bot.id.clone()))
            .unwrap();
        assert!(primary.ends_with("settings/swift.json"));
        drop(tmp);
    }
}
