//! Inbound webhooks: `POST /bird/<id>` and `POST /room/<id>` on localhost,
//! bearer-token gated — CI failures and external events wake birds without a
//! human in the loop. Off unless config declares a `webhook` block. Hand-rolled
//! HTTP/1.1 on std's TcpListener: one tiny thread, no new dependencies.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::Sender;
use std::thread;

use crate::config::WebhookConfig;
use crate::event::Event;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WebhookTarget {
    Bird(String),
    Room(String),
}

#[derive(Debug)]
pub struct Webhook {
    pub target: WebhookTarget,
    pub text: String,
}

pub fn spawn(cfg: WebhookConfig, tx: Sender<Event>) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", cfg.port))?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            let token = cfg.token.clone();
            // One thread per request; traffic is a trickle by design.
            thread::spawn(move || handle(stream, &token, &tx));
        }
    });
    Ok(())
}

fn handle(mut stream: std::net::TcpStream, token: &str, tx: &Sender<Event>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until the request is parseable or caps are hit.
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 64 * 1024 {
                    break;
                }
                if request_complete(&buf) {
                    break;
                }
            }
        }
    }
    let (code, reason) = match parse_request(&buf, token) {
        Ok(hook) => {
            let _ = tx.send(Event::Webhook(Box::new(hook)));
            (204, "No Content")
        }
        Err(code) => (
            code,
            match code {
                400 => "Bad Request",
                401 => "Unauthorized",
                404 => "Not Found",
                405 => "Method Not Allowed",
                _ => "Error",
            },
        ),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
    );
}

fn request_complete(buf: &[u8]) -> bool {
    let Some(header_end) = find_header_end(buf) else {
        return false;
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]);
    let len = content_length(&headers).unwrap_or(0);
    buf.len() >= header_end + 4 + len
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(headers: &str) -> Option<usize> {
    headers
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(str::trim).map(String::from))
        .and_then(|v| v.parse().ok())
}

/// Pure request → webhook translation (unit-tested). Error = HTTP status.
pub fn parse_request(buf: &[u8], token: &str) -> Result<Webhook, u16> {
    let header_end = find_header_end(buf).ok_or(400u16)?;
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().ok_or(400u16)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(400u16)?;
    let path = parts.next().ok_or(400u16)?;
    if method != "POST" {
        return Err(405);
    }

    // Scheme is case-insensitive; the token itself matches exactly.
    let authorized = head.lines().any(|l| {
        let Some((name, value)) = l.split_once(':') else {
            return false;
        };
        if !name.trim().eq_ignore_ascii_case("authorization") {
            return false;
        }
        match value.trim().split_once(' ') {
            Some((scheme, tok)) => scheme.eq_ignore_ascii_case("bearer") && tok.trim() == token,
            None => false,
        }
    });
    if !authorized {
        return Err(401);
    }

    let target = if let Some(id) = path.strip_prefix("/bird/") {
        WebhookTarget::Bird(id.trim_matches('/').to_string())
    } else if let Some(id) = path.strip_prefix("/room/") {
        WebhookTarget::Room(id.trim_matches('/').to_string())
    } else {
        return Err(404);
    };

    let len = content_length(&head).unwrap_or(0);
    let body_start = header_end + 4;
    let body = buf
        .get(body_start..body_start + len)
        .map(|b| String::from_utf8_lossy(b).trim().to_string())
        .unwrap_or_default();
    if body.is_empty() {
        return Err(400);
    }
    Ok(Webhook { target, text: body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, auth: Option<&str>, body: &str) -> Vec<u8> {
        let auth_line = auth.map(|a| format!("Authorization: Bearer {a}\r\n")).unwrap_or_default();
        format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\n{auth_line}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn routes_bird_and_room() {
        let hook = parse_request(&req("POST", "/bird/raven", Some("t0k"), "CI red on main"), "t0k").unwrap();
        assert_eq!(hook.target, WebhookTarget::Bird("raven".into()));
        assert_eq!(hook.text, "CI red on main");
        let hook = parse_request(&req("POST", "/room/fly-calc", Some("t0k"), "build broke"), "t0k").unwrap();
        assert_eq!(hook.target, WebhookTarget::Room("fly-calc".into()));
    }

    #[test]
    fn rejects_bad_auth_method_path_and_empty_body() {
        assert_eq!(parse_request(&req("POST", "/bird/raven", Some("wrong"), "x"), "t0k").unwrap_err(), 401);
        assert_eq!(parse_request(&req("POST", "/bird/raven", None, "x"), "t0k").unwrap_err(), 401);
        assert_eq!(parse_request(&req("GET", "/bird/raven", Some("t0k"), "x"), "t0k").unwrap_err(), 405);
        assert_eq!(parse_request(&req("POST", "/nope", Some("t0k"), "x"), "t0k").unwrap_err(), 404);
        assert_eq!(parse_request(&req("POST", "/bird/raven", Some("t0k"), ""), "t0k").unwrap_err(), 400);
    }
}
