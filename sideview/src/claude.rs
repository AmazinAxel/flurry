// The `flurry:claude` page: a local chat (claude/index.html) backed by Claude Code in print mode on
// the user's subscription. The page talks to us through console messages (\u{1}claude:user <parts>,
// \u{1}claude:stop, \u{1}claude:new, \u{1}open:<url>); we build the stream-json input from those,
// and its output lines go back as data: window.__claude([<line as a JSON string>, ...]).

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const URL: &str = "flurry:claude";

const SYSTEM: &str = r#"Caveman, fewest words, every reply. No articles, "I", filler, hedges, praise. Not: "The sky looks blue because air scatters short wavelengths more." But: "Air scatters blue most."
Think full, talk small: keep cause, caveat, disagreement; wrong premise → say so. Answer first. Default 1 line, cap 3 unless detail asked; asked → full, still terse.
No headers, bold, lists, tables, recap, offers, unless asked. Unsure → say so, never invent.
Code, numbers, names, errors exact, in fenced blocks. Math LaTeX: $...$, $$...$$."#;

/// Private to the user: never a shared directory like /tmp.
fn dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|run| std::path::PathBuf::from(run).join("flurry-claude"))
}

/// A JSON string literal as JSON.stringify writes it: no raw control characters or newlines.
fn json_literal(s: &str) -> Option<&str> {
    let mut it = s.strip_prefix('"')?.strip_suffix('"')?.chars();
    while let Some(c) = it.next() {
        match c {
            '"' | '\0'..='\x1f' => return None,
            '\\' => match it.next()? {
                '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' => {}
                'u' => {
                    for _ in 0..4 {
                        if !it.next()?.is_ascii_hexdigit() {
                            return None;
                        }
                    }
                }
                _ => return None,
            },
            _ => {}
        }
    }
    Some(s)
}

/// The page's `claude:user` payload: ` img:<type>:<base64>` parts, then an optional
/// ` text:<JSON string>`, as one stream-json user message.
pub fn user_message(mut parts: &str) -> Option<String> {
    let mut content = vec![];
    while let Some(rest) = parts.strip_prefix(" img:") {
        let (part, next) = rest.split_at(rest.find(' ').unwrap_or(rest.len()));
        let (mime, data) = part.split_once(':')?;
        let ok_mime = matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp");
        let ok_data = !data.is_empty() && data.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=');
        if !ok_mime || !ok_data {
            return None;
        }
        content.push(format!(r#"{{"type":"image","source":{{"type":"base64","media_type":"{mime}","data":"{data}"}}}}"#));
        parts = next;
    }
    if let Some(text) = parts.strip_prefix(" text:") {
        content.push(format!(r#"{{"type":"text","text":{}}}"#, json_literal(text)?));
    } else if !parts.is_empty() {
        return None;
    }
    if content.is_empty() {
        return None;
    }
    Some(format!(r#"{{"type":"user","message":{{"role":"user","content":[{}]}}}}"#, content.join(",")))
}

/// `s` as a JSON (and JS) string literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The page is read from FLURRY_CLAUDE_PAGE (set by the package wrapper; kept out of the binary so
/// page edits don't rebuild Servo), else from the source tree.
pub fn page_url() -> Option<url::Url> {
    let page = std::env::var_os("FLURRY_CLAUDE_PAGE").map(std::path::PathBuf::from);
    let page = page.unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/claude").into());
    url::Url::from_file_path(page.join("index.html")).ok()
}

#[derive(Default)]
pub struct Chat {
    proc: Option<(Child, ChildStdin, ChildStdout)>,
    buf: Vec<u8>,
    stops: u32,
}

impl Chat {
    pub fn fd(&self) -> RawFd {
        self.proc.as_ref().map_or(-1, |(_, _, out)| out.as_raw_fd())
    }

    fn spawn() -> Option<(Child, ChildStdin, ChildStdout)> {
        let home = std::env::var("HOME").unwrap_or_default();
        let user = std::env::var("USER").unwrap_or_default();
        // the service PATH may lack the user profile
        let bins = ["claude".to_string(), format!("/etc/profiles/per-user/{user}/bin/claude"), format!("{home}/.nix-profile/bin/claude"), format!("{home}/.local/bin/claude")];
        let cwd = dir()?;
        let _ = std::fs::create_dir_all(&cwd);
        bins.iter().find_map(|bin| {
            let mut cmd = Command::new(bin);
            // dies with the helper, however that goes
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
            let mut child = cmd
                // for short questions: no user settings, CLAUDE.md, hooks, claude.ai connectors or default
                // system prompt, so the first turn costs ~650 tokens, not ~38000;
                // sessions are saved under this cwd, so `claude -c` / `-r` there continues them
                .args(["-p", "--tools", "", "--setting-sources", "", "--strict-mcp-config"])
                .args(["--model", "opus", "--effort", "medium"])
                // summarized thinking for the status line (billed as thinking already is: no extra tokens)
                .args(["--settings", r#"{"showThinkingSummaries":true}"#])
                .args(["--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"])
                .args(["--system-prompt", SYSTEM])
                .current_dir(&cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let (stdin, stdout) = (child.stdin.take()?, child.stdout.take()?);
            unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) | libc::O_NONBLOCK) };
            Some((child, stdin, stdout))
        })
    }

    /// Starts Claude Code ahead of the first message (the launch is most of the first reply's wait).
    pub fn prewarm(&mut self) {
        if self.proc.is_none() {
            self.proc = Self::spawn();
        }
    }

    fn write(&mut self, line: &str) -> bool {
        let Some((_, stdin, _)) = &mut self.proc else { return false };
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.write_all(b"\n")).and_then(|_| stdin.flush()).is_ok()
    }

    /// One stream-json user message (see `user_message`); the conversation lives in the running
    /// process. One that has died since starts over in a fresh one.
    pub fn send(&mut self, line: &str) -> bool {
        self.prewarm();
        if self.write(line) {
            return true;
        }
        self.reset();
        self.prewarm();
        self.write(line)
    }

    /// Ctrl+C: stops the reply; the conversation stays.
    pub fn interrupt(&mut self) {
        self.stops += 1;
        let line = format!(r#"{{"type":"control_request","request_id":"stop{}","request":{{"subtype":"interrupt"}}}}"#, self.stops);
        self.write(&line);
    }

    /// Ctrl+N: the next message starts a fresh process (and conversation).
    pub fn reset(&mut self) {
        if let Some((mut child, _, _)) = self.proc.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.buf.clear();
    }

    /// Complete event lines read so far; an `exit` event once the process is gone.
    pub fn read(&mut self) -> Vec<String> {
        let mut lines = vec![];
        let Some((_, _, out)) = &mut self.proc else { return lines };
        let mut chunk = [0u8; 16384];
        let eof = loop {
            match out.read(&mut chunk) {
                Ok(0) => break true,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break false,
                Err(_) => break true,
            }
        };
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            // only what the page renders: text/thinking deltas and the end of a turn
            if line.contains("\"text_delta\"") || line.contains("\"thinking_delta\"") || line.contains("\"type\":\"result\"") {
                lines.push(line);
            }
        }
        if eof {
            self.reset();
            lines.push(r#"{"type":"exit"}"#.into());
        }
        lines
    }
}

impl Drop for Chat {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_messages() {
        assert_eq!(
            user_message(r#" text:"hi \"x\"\n""#).as_deref(),
            Some(r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi \"x\"\n"}]}}"#)
        );
        assert!(user_message(" img:image/png:AAAA= text:\"a b\"").unwrap().contains(r#""media_type":"image/png","data":"AAAA=""#));
        assert!(user_message("").is_none());
        assert!(user_message(" text:\"a\nb\"").is_none()); // raw newline
        assert!(user_message(r#" text:"a"}, {"x":"y""#).is_none());
        assert!(user_message(r#" text:"a\""#).is_none());
        assert!(user_message(" img:image/svg+xml:AAAA").is_none());
        assert!(user_message(" img:image/png:AA\"A").is_none());
        assert!(user_message(" foo").is_none());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("a\"\\\n\u{1}\u{2028}"), r#""a\"\\\n\u0001\u2028""#);
    }
}
