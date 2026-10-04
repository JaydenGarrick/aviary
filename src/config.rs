//! Config, personas, rooms, and state — all under one directory.
//!
//! `~/.config/aviary/` (override: `AVIARY_CONFIG_DIR`):
//!   config.json   bots + rooms (user- AND app-edited; pretty-printed)
//!   birds/*.md    persona files, materialized on first run, user-editable
//!   mcp.json      MCP servers every bird spawns with (Linear + Figma hooks)
//!   rooms/*.md    group-chat transcripts (append-only)
//!   handoffs/     where birds park large handoff briefs
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
    #[serde(default)]
    bots: Vec<Bot>,
    #[serde(default)]
    rooms: Vec<Room>,
    #[serde(default)]
    webhook: Option<WebhookConfig>,
}

pub struct Config {
    pub dir: PathBuf,
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

impl Config {
    /// Load the config, scaffolding the whole directory on first run.
    /// Existing installs still get newly-shipped default files (personas,
    /// templates) materialized — per-file, never overwriting edits.
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
        }

        Ok(Config {
            dir,
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
            bots: self.bots.clone(),
            rooms: self.rooms.clone(),
            webhook: self.webhook.clone(),
        };
        let json = serde_json::to_string_pretty(&raw)?;
        std::fs::write(self.dir.join("config.json"), json + "\n")?;
        Ok(())
    }

    /// Flip a bird's notification toggle and persist it.
    pub fn set_notify(&mut self, id: &BotId, on: bool) -> Result<()> {
        if let Some(bot) = self.bots.iter_mut().find(|b| &b.id == id) {
            bot.notify = on;
            self.save()?;
        }
        Ok(())
    }

    /// The per-session `--settings` file for one bird: Stop + Notification
    /// hooks (the truthful done/needs-input signals, probed working via
    /// `--settings`), merged with the bird's optional permission allowlist.
    /// Regenerated every launch — the hook command embeds the current exe.
    pub fn settings_file_for(&self, bot: &Bot) -> Result<PathBuf> {
        let dir = self.dir.join("settings");
        std::fs::create_dir_all(&dir)?;
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "aviary".into());
        let hook = serde_json::json!([{ "hooks": [{
            "type": "command",
            "command": format!("\"{exe}\" --hook"),
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

        let path = dir.join(format!("{}.json", bot.id));
        std::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n")?;
        Ok(path)
    }

    /// Where `aviary --hook` appends events and the shell reads them.
    pub fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
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

/// Directory tree + every shipped default file, written only when missing —
/// safe to run on every startup, so upgrades deliver new personas/templates
/// without ever overwriting a user's edits.
fn materialize_defaults(dir: &Path) -> Result<()> {
    for sub in ["birds", "rooms", "handoffs"] {
        std::fs::create_dir_all(dir.join(sub))
            .with_context(|| format!("cannot create {}", dir.join(sub).display()))?;
    }

    for (name, body) in [
        ("swift.md", include_str!("../assets/birds/swift.md")),
        ("weaver.md", include_str!("../assets/birds/weaver.md")),
        ("raven.md", include_str!("../assets/birds/raven.md")),
        ("mimus.md", include_str!("../assets/birds/mimus.md")),
    ] {
        let path = dir.join("birds").join(name);
        if !path.is_file() {
            std::fs::write(&path, body)?;
        }
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

    Ok(())
}

/// First run only: the default Blackbird flock.
fn scaffold(dir: &Path) -> Result<()> {
    let default_config = RawConfig {
        bots: vec![
            make_bot(
                BotId("swift".into()),
                "Swift",
                "🪶",
                "~/Library/Develop/iOS/ios",
                "birds/swift.md",
            ),
            make_bot(
                BotId("weaver".into()),
                "Weaver",
                "🧺",
                "~/Library/Develop/iOS/android",
                "birds/weaver.md",
            ),
            make_bot(
                BotId("raven".into()),
                "Raven",
                "🐦‍⬛",
                "~/Library/Develop/Backend/core-api",
                "birds/raven.md",
            ),
        ],
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
}

impl State {
    pub fn load(dir: &Path) -> State {
        std::fs::read_to_string(dir.join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn spawned_once(&self, id: &BotId) -> bool {
        self.spawned.contains(&id.0)
    }

    pub fn mark_spawned(&mut self, dir: &Path, id: &BotId) {
        self.spawned.insert(id.0.clone());
        self.write(dir);
    }

    /// A `--resume` that died instantly means the named session is gone —
    /// forget it so the next launch starts fresh.
    pub fn forget(&mut self, dir: &Path, id: &BotId) {
        self.spawned.remove(&id.0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config() -> (tempfile::TempDir, Config) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load_or_scaffold(tmp.path().to_path_buf()).unwrap();
        (tmp, cfg)
    }

    #[test]
    fn scaffold_creates_three_birds_and_hooks() {
        let (tmp, cfg) = temp_config();
        assert_eq!(cfg.bots.len(), 3);
        assert_eq!(cfg.bots[0].id.0, "swift");
        assert!(tmp.path().join("birds/raven.md").is_file());
        assert!(tmp.path().join("mcp.json").is_file());
        assert!(tmp.path().join("rooms").is_dir());
        assert!(tmp.path().join("handoffs").is_dir());
        // Reload reads what scaffold wrote.
        let again = Config::load_or_scaffold(tmp.path().to_path_buf()).unwrap();
        assert_eq!(again.bots.len(), 3);
    }

    #[test]
    fn add_bot_materializes_persona_and_persists() {
        let (tmp, mut cfg) = temp_config();
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
        assert_eq!(again.bots.len(), 4);
    }

    #[test]
    fn add_bot_rejects_duplicates_and_bad_repos() {
        let (tmp, mut cfg) = temp_config();
        assert!(cfg
            .add_bot("swift", "Swift", "x", tmp.path().to_str().unwrap())
            .is_err());
        assert!(cfg.add_bot("newbie", "N", "x", "/definitely/not/a/dir").is_err());
    }

    #[test]
    fn add_room_touches_transcript_and_validates_members() {
        let (tmp, mut cfg) = temp_config();
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
        let id = BotId("swift".into());
        let mut st = State::default();
        assert!(!st.spawned_once(&id));
        st.mark_spawned(tmp.path(), &id);
        assert!(State::load(tmp.path()).spawned_once(&id));
        st.forget(tmp.path(), &id);
        assert!(!State::load(tmp.path()).spawned_once(&id));
    }
}
