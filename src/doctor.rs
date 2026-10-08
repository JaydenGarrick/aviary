//! `aviary doctor` — one screen of ✓/✗ that explains every known foot-gun
//! before it costs a session: claude version, repos, config, hooks, events.

use std::process::Command;

use anyhow::Result;

use crate::config::{self, Config};

const MIN_CLAUDE: (u32, u32, u32) = (2, 1, 224); // --name/--resume + messaging
/// Flock workers: `--bg` · `attach` · `stop` · `rm` · `--plugin-dir`, verified here.
const FLOCK_CLAUDE: (u32, u32, u32) = (2, 1, 289);

pub fn run() -> Result<()> {
    println!("aviary doctor\n");

    // claude version
    let mut flock_ok = false;
    match claude_version() {
        Some((v, n)) => {
            row(n >= MIN_CLAUDE, &format!("claude {v} (need ≥ 2.1.224 for named sessions + messaging)"));
            flock_ok = n >= FLOCK_CLAUDE;
        }
        None => row(false, "claude not found on PATH"),
    }

    // config + scaffold
    let dir = config::default_dir()?;
    println!("  · config dir {}", dir.display());
    let cfg = match Config::load_or_scaffold(dir.clone()) {
        Ok(c) => {
            row(true, &format!("config.json parses — {} birds, {} rooms", c.bots.len(), c.rooms.len()));
            Some(c)
        }
        Err(e) => {
            row(false, &format!("config: {e:#}"));
            None
        }
    };

    if let Some(cfg) = &cfg {
        for bot in &cfg.bots {
            let repo = bot.repo_path();
            row(
                repo.is_dir(),
                &format!("{} {} repo {}", bot.glyph, bot.id, repo.display()),
            );
            if let Some(p) = &bot.permissions {
                row(true, &format!("    permissions allowlist: {p}"));
            }
            for r in &bot.routines {
                match crate::routine::parse(&r.schedule) {
                    Ok(_) => row(true, &format!("    routine {} — {}", r.id, r.schedule)),
                    Err(e) => row(false, &format!("    routine {} — {e:#}", r.id)),
                }
            }
        }

        row(cfg.mcp_config_path().is_file(), "mcp.json (Linear + Figma hooks; OAuth is PER-USER — authorize once, every bird is covered)");

        // Flock: workers are claude --bg sessions a bird spawns itself.
        row(flock_ok, "claude ≥ 2.1.289 for flock workers (--bg · attach · stop · --plugin-dir)");
        let plugin = cfg.plugin_dir();
        let mut plugin_ok = plugin.join(".claude-plugin").join("plugin.json").is_file();
        let mut notes = Vec::new();
        for (name, shipped) in config::SHIPPED_SKILLS {
            let path = plugin.join("skills").join(name).join("SKILL.md");
            match std::fs::read_to_string(&path) {
                Ok(text) if text == shipped => {}
                Ok(_) => notes.push(format!(
                    "{name}/SKILL.md differs from the shipped copy (edited, or an older release — delete it to refresh)"
                )),
                Err(_) => {
                    plugin_ok = false;
                    notes.push(format!("{name}/SKILL.md missing (restart aviary to materialize)"));
                }
            }
        }
        row(plugin_ok, &format!("flock plugin {} (rides --plugin-dir into every bird; the orchestrator passes it to workers)", plugin.display()));
        for n in notes {
            println!("      · {n}");
        }
        let reports = cfg.reports_dir();
        let probe = reports.join(".doctor-probe");
        let writable = std::fs::create_dir_all(&reports).is_ok() && std::fs::write(&probe, "ok").is_ok();
        let _ = std::fs::remove_file(&probe);
        row(writable, &format!("reports/ writable — workers drop <name>.md here ({})", reports.display()));

        // Hooks: per-session via --settings (verified working on this machine);
        // the settings files regenerate at every launch with the current exe.
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        row(true, &format!("hooks ride --settings per session · hook cmd: {exe} --hook --session <name>"));
        if exe.contains("/target/") {
            println!("      ⚠ that's a build-dir path — `cargo install --path .` gives birds a stable hook binary");
        }

        let events = cfg.events_path();
        match std::fs::metadata(&events) {
            Ok(m) => row(true, &format!("events.jsonl {} bytes", m.len())),
            Err(_) => row(true, "events.jsonl not created yet (appears on the first hook)"),
        }

        match &cfg.webhook {
            Some(w) => row(true, &format!("webhooks ON — 127.0.0.1:{} (POST /bird/<id> · /room/<id>)", w.port)),
            None => println!("  · webhooks off — add {{\"webhook\":{{\"port\":4242,\"token\":\"…\"}}}} to config.json to enable"),
        }

        // agents-json reachability (+ how many flock workers it names right now)
        let agents = Command::new("claude")
            .args(["agents", "--json"])
            .output()
            .ok()
            .filter(|o| o.status.success());
        match agents {
            Some(o) => {
                let rows: Vec<crate::command::SessionInfo> =
                    serde_json::from_slice(&o.stdout).unwrap_or_default();
                let workers = rows
                    .iter()
                    .filter(|r| {
                        crate::flock::WorkerName::parse(&r.name).is_some_and(|w| cfg.bot(&w.bot).is_some())
                    })
                    .count();
                row(true, &format!(
                    "claude agents --json reachable — {} sessions, {workers} flock workers attributed",
                    rows.len()
                ));
            }
            None => row(false, "claude agents --json reachable (authoritative busy/idle/needs_input)"),
        }

        if cfg.bots.is_empty() {
            let hatch = crate::keymap::label_for(&[crate::keymap::GLOBAL], crate::action::Action::NewBot)
                .unwrap_or_default();
            println!(
                "\n  · no birds yet — press {hatch} in the cockpit, or add to config.json bots:\n  \
                 {{ \"id\": \"<id>\", \"name\": \"<Name>\", \"glyph\": \"🐦\", \"repo\": \"~/path/to/repo\", \"persona\": \"birds/<id>.md\" }}"
            );
        }
    }

    println!("\n  one-time per repo: claude's workspace-trust dialog has no pre-acceptance — answer it on each bird's first flight");
    Ok(())
}

fn row(ok: bool, label: &str) {
    println!("  {} {label}", if ok { "✓" } else { "✗" });
}

fn claude_version() -> Option<(String, (u32, u32, u32))> {
    let out = Command::new("claude").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.split_whitespace().next()?.to_string();
    let mut parts = version.split('.').filter_map(|p| p.parse::<u32>().ok());
    let v = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    Some((version, v))
}
