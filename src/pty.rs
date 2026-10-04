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
use std::sync::{Arc, RwLock};
use std::thread;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

use crate::config::BotId;
use crate::event::Event;

pub struct Terminal {
    pub parser: Arc<RwLock<vt100::Parser>>,
    running: Arc<AtomicBool>,
    writer: Box<dyn std::io::Write + Send>,
    master: Box<dyn MasterPty + Send>,
    size: (u16, u16),
}

impl Terminal {
    /// Spawn `program` on a PTY of the given size, in `cwd`. The reader thread
    /// pings the UI channel with the owning bot's id after every chunk, so the
    /// roster knows WHO spoke without polling.
    pub fn spawn(
        bot: BotId,
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
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

        let running = Arc::new(AtomicBool::new(true));

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("could not start `{program}`"))?;
        drop(pair.slave);

        {
            let running = running.clone();
            let notify = notify.clone();
            let bot = bot.clone();
            thread::spawn(move || {
                let _ = child.wait();
                running.store(false, Ordering::Relaxed);
                let _ = notify.send(Event::AgentOutput(bot));
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
                            if notify.send(Event::AgentOutput(bot.clone())).is_err() {
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
            writer,
            master: pair.master,
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
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
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
            let _ = self.writer.write_all(&payload);
            let _ = self.writer.flush();
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
