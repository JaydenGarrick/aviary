//! `aviary doctor` — one screen of ✓/✗ that explains every known foot-gun
//! before it costs a session: claude version, repos, config, hooks, events.

use std::process::Command;

use anyhow::Result;

use crate::config::{self, Config};

const MIN_CLAUDE: (u32, u32, u32) = (2, 1, 224); // --name/--resume + messaging

pub fn run() -> Result<()> {
    println!("aviary doctor\n");

    // claude version
    match claude_version() {
        Some((v, ok)) => row(ok, &format!("claude {v} (need ≥ 2.1.224 for named sessions + messaging)")),
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

        // agents-json reachability
        let agents_ok = Command::new("claude")
            .args(["agents", "--json"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        row(agents_ok, "claude agents --json reachable (authoritative busy/idle/needs_input)");

        if !cfg.bots.iter().any(|b| b.id.0 == "mimus") {
            println!(
                "\n  tip: hatch mimus 🪞 (the mockingbird parity oracle) — add to config.json bots:\n  \
                 {{ \"id\": \"mimus\", \"name\": \"Mimus\", \"glyph\": \"🪞\", \"repo\": \"~/Library/Develop/iOS/mockingbird\", \"persona\": \"birds/mimus.md\" }}"
            );
        }
    }

    println!("\n  one-time per repo: claude's workspace-trust dialog has no pre-acceptance — answer it on each bird's first flight");
    Ok(())
}

fn row(ok: bool, label: &str) {
    println!("  {} {label}", if ok { "✓" } else { "✗" });
}

fn claude_version() -> Option<(String, bool)> {
    let out = Command::new("claude").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.split_whitespace().next()?.to_string();
    let mut parts = version.split('.').filter_map(|p| p.parse::<u32>().ok());
    let v = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    Some((version, v >= MIN_CLAUDE))
}
