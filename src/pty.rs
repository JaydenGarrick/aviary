//! An embedded terminal: a child process on a real pseudo-terminal, parsed
//! into a cell grid ratatui can draw.
//!
//! Claude Code is itself a full-screen TUI — raw mode, cursor addressing, alt
//! screen, bracketed paste — so a plain pipe would reduce it to dumb line
//! output. `portable-pty` gives the child a terminal it believes in, `vt100`
//! turns its escape stream back into cells, and `tui-term` draws them.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

use crate::config::SessionKey;
use crate::event::Event;

/// How long after an injected prompt before its Enter is sent. Claude Code
/// detects a rapid input burst as a PASTE, and an Enter inside the burst is
/// swallowed as paste content instead of submitting — the delay makes it a
/// real keystroke.
const SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

pub struct Terminal {
    pub parser: Arc<RwLock<vt100::Parser>>,
    running: Arc<AtomicBool>,
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    /// Hangs the child up when the terminal is dropped. Closing our master
    /// is NOT enough: the reader thread holds a dup of it (blocked in
    /// `read`), so the child never sees the hangup and outlives its pane.
    killer: Box<dyn ChildKiller + Send + Sync>,
    size: (u16, u16),
}

impl Drop for Terminal {
    /// Dropping a session means hanging it up (stop, fresh start, a
    /// conversation switch) — SIGHUP, which claude answers by saving and
    /// exiting. A child that already exited makes this a no-op.
    fn drop(&mut self) {
        if self.running.load(Ordering::Relaxed) {
            let _ = self.killer.kill();
        }
    }
}

impl Terminal {
    /// Spawn `program` on a PTY of the given size, in `cwd`, with `env` set
    /// over the inherited environment. The reader thread pings the UI channel
    /// with the owning session's key after every chunk, so the roster knows
    /// WHO spoke without polling.
    #[allow(clippy::too_many_arguments)] // a spawn: every part of the child is its own argument
    pub fn spawn(
        key: SessionKey,
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
        env: &[(&str, String)],
        rows: u16,
        cols: u16,
        notify: Sender<Event>,
    ) -> Result<Terminal> {
        let (rows, cols) = (rows.max(4), cols.max(20));

        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("could not open a pseudo-terminal")?;

        let mut cmd = CommandBuilder::new(program);
        for a in args {
            cmd.arg(a);
        }
        cmd.cwd(cwd);
        // Claim a colour-capable terminal so the child does not fall back to
        // its monochrome rendering path.
        cmd.env("TERM", "xterm-256color");
        // The caller's own variables (the config dir for the in-session mod);
        // children the child spawns — flock workers — inherit them too.
        for (name, value) in env {
            cmd.env(name, value);
        }

        let running = Arc::new(AtomicBool::new(true));

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("could not start `{program}`"))?;
        drop(pair.slave);
        let killer = child.clone_killer();

        {
            let running = running.clone();
            let notify = notify.clone();
            let key = key.clone();
            thread::spawn(move || {
                let _ = child.wait();
                running.store(false, Ordering::Relaxed);
                let _ = notify.send(Event::AgentOutput(key));
            });
        }

        let parser = Arc::new(RwLock::new(vt100::Parser::new(rows, cols, 10_000)));
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("could not read from the pseudo-terminal")?;

        {
            let parser = parser.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    // A full read would block until EOF; take what is there.
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut p) = parser.write() {
                                p.process(&buf[..n]);
                            }
                            // The run loop coalesces these — a chatty child
                            // cannot spin the renderer.
                            if notify.send(Event::AgentOutput(key.clone())).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        let writer = pair
            .master
            .take_writer()
            .context("could not write to the pseudo-terminal")?;

        Ok(Terminal {
            parser,
            running,
            writer: Arc::new(Mutex::new(writer)),
            master: pair.master,
            killer,
            size: (rows, cols),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Keep the child's idea of the window in step with the pane. Skipping
    /// this is the classic embedded-terminal bug: the child renders to a stale
    /// width and the pane fills with wrapped garbage.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(4), cols.max(20));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.write() {
            p.screen_mut().set_size(rows, cols);
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        use std::io::Write;
        // Typing always snaps back to live output — reading history and
        // talking to the child are different moments.
        self.snap_live();
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Inject a PROMPT: the text lands now, the Enter follows on its own
    /// after [`SUBMIT_DELAY`] so the child's paste detection can't swallow it.
    pub fn send_line(&mut self, text: &str) {
        self.send(text.as_bytes());
        let writer = self.writer.clone();
        thread::spawn(move || {
            thread::sleep(SUBMIT_DELAY);
            use std::io::Write;
            if let Ok(mut w) = writer.lock() {
                let _ = w.write_all(b"\r");
                let _ = w.flush();
            }
        });
    }

    pub fn send_key(&mut self, key: KeyEvent) {
        if let Some(bytes) = encode(key) {
            self.send(&bytes);
        }
    }

    // ------------------------------------------------------------ scrollback
    //
    // Two worlds, like a real terminal: Claude Code runs on the ALTERNATE
    // screen with mouse reporting on (?1049h + ?1000/1002/1006h) and scrolls
    // its own transcript, so the wheel is FORWARDED as SGR mouse sequences.
    // On the primary screen the wheel moves vt100's own scrollback instead.

    /// Wheel over the pane. `col`/`row` are 0-based cell coords inside the
    /// child's screen; positive `lines` scrolls back/up.
    pub fn wheel(&mut self, lines: i32, col: u16, row: u16) {
        let alt = self
            .parser
            .read()
            .map(|p| p.screen().alternate_screen())
            .unwrap_or(false);

        if alt {
            // SGR mouse: button 64 = wheel up, 65 = wheel down; 1-based coords.
            let button = if lines > 0 { 64 } else { 65 };
            let seq = format!("\x1b[<{};{};{}M", button, col + 1, row + 1);
            let payload = seq.into_bytes().repeat(lines.unsigned_abs() as usize);
            // Straight to the writer — send() would pointlessly snap a
            // scrollback that cannot exist on the alt screen.
            use std::io::Write;
            if let Ok(mut w) = self.writer.lock() {
                let _ = w.write_all(&payload);
                let _ = w.flush();
            }
            return;
        }

        if let Ok(mut p) = self.parser.write() {
            let cur = p.screen().scrollback();
            let next = if lines > 0 {
                cur.saturating_add(lines as usize)
            } else {
                cur.saturating_sub((-lines) as usize)
            };
            p.screen_mut().set_scrollback(next);
        }
    }

    /// How far back the view currently sits (0 = live).
    pub fn scrolled(&self) -> usize {
        self.parser
            .read()
            .map(|p| p.screen().scrollback())
            .unwrap_or(0)
    }

    pub fn snap_live(&self) {
        if let Ok(mut p) = self.parser.write() {
            p.screen_mut().set_scrollback(0);
        }
    }
}

/// crossterm key event → the bytes a terminal would have sent.
fn encode(key: KeyEvent) -> Option<Vec<u8>> {
    use KeyCode as K;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut out = match key.code {
        K::Char(c) if ctrl => {
            let lower = c.to_ascii_lowercase();
            match lower {
                'a'..='z' => vec![lower as u8 - b'a' + 1],
                ' ' | '@' => vec![0],
                '[' => vec![0x1b],
                '\\' => vec![0x1c],
                ']' => vec![0x1d],
                '^' => vec![0x1e],
                '_' | '?' => vec![0x1f],
                _ => c.to_string().into_bytes(),
            }
        }
        K::Char(c) => c.to_string().into_bytes(),
        K::Enter => vec![b'\r'],
        K::Tab => vec![b'\t'],
        K::BackTab => b"\x1b[Z".to_vec(),
        K::Backspace => vec![0x7f],
        K::Esc => vec![0x1b],
        K::Up => b"\x1b[A".to_vec(),
        K::Down => b"\x1b[B".to_vec(),
        K::Right => b"\x1b[C".to_vec(),
        K::Left => b"\x1b[D".to_vec(),
        K::Home => b"\x1b[H".to_vec(),
        K::End => b"\x1b[F".to_vec(),
        K::PageUp => b"\x1b[5~".to_vec(),
        K::PageDown => b"\x1b[6~".to_vec(),
        K::Delete => b"\x1b[3~".to_vec(),
        K::Insert => b"\x1b[2~".to_vec(),
        K::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + (n - 1)],
        K::F(n @ 5..=12) => {
            const CODES: [&[u8]; 8] = [b"15", b"17", b"18", b"19", b"20", b"21", b"23", b"24"];
            let mut v = b"\x1b[".to_vec();
            v.extend_from_slice(CODES[(n - 5) as usize]);
            v.push(b'~');
            v
        }
        _ => return None,
    };

    if alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}
