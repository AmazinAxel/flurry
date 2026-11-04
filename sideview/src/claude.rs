// The `flurry:claude` page: a local chat (claude/index.html) backed by Claude Code in print mode on
// the user's subscription. The page talks to us through console messages (\u{1}claude:user <parts>,
// \u{1}claude:stop, \u{1}claude:new, \u{1}claude:fast / full, \u{1}open:<url>); we build the stream-json input from those,
// and its output lines go back as data: window.__claude([<line as a JSON string>, ...]).
// std + libc only: flurry's Cargo.toml also builds it as a test target (CI can't build Servo).
#![cfg_attr(test, allow(dead_code))]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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

/// Where `claude` is: $PATH, then the user profiles (the service PATH may lack them).
fn find_bin() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let user = std::env::var("USER").unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let profiles = [Some(PathBuf::from(format!("/etc/profiles/per-user/{user}/bin"))), home.as_ref().map(|h| h.join(".nix-profile/bin")), home.map(|h| h.join(".local/bin"))];
    std::env::split_paths(&path)
        .chain(profiles.into_iter().flatten())
        .map(|d| d.join("claude"))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

#[derive(Default)]
pub struct Chat {
    proc: Option<(Child, ChildStdin, ChildStdout)>,
    buf: Vec<u8>,
    /// input not yet taken by Claude Code from `sent` on (stdin is nonblocking: a big image must
    /// not stall the panel); emptied once all of it went
    out: Vec<u8>,
    sent: usize,
    /// found once, looked up again only if it stops working
    bin: Option<PathBuf>,
    stops: u32,
    /// Opus at medium effort; else (the default) Fast: Sonnet at low effort
    full: bool,
}

impl Chat {
    pub fn fd(&self) -> RawFd {
        self.proc.as_ref().map_or(-1, |(_, _, out)| out.as_raw_fd())
    }

    /// Claude Code's stdin while input waits for room in the pipe, else -1 (ignored by poll).
    pub fn write_fd(&self) -> RawFd {
        match &self.proc {
            Some((_, stdin, _)) if !self.out.is_empty() => stdin.as_raw_fd(),
            _ => -1,
        }
    }

    fn spawn(&mut self) -> Option<(Child, ChildStdin, ChildStdout)> {
        let cwd = dir()?;
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&cwd);
        if self.bin.is_none() {
            self.bin = find_bin();
        }
        let proc = Self::spawn_bin(self.bin.as_ref()?, &cwd, self.full);
        if proc.is_none() {
            self.bin = None; // gone since: looked up again next time
        }
        proc
    }

    fn spawn_bin(bin: &Path, cwd: &Path, full: bool) -> Option<(Child, ChildStdin, ChildStdout)> {
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
            .args(if full { ["--model", "opus", "--effort", "medium"] } else { ["--model", "sonnet", "--effort", "low"] })
            // summarized thinking for the status line (billed as thinking already is: no extra tokens)
            .args(["--settings", r#"{"showThinkingSummaries":true}"#])
            .args(["--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"])
            .args(["--system-prompt", SYSTEM])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let (stdin, stdout) = (child.stdin.take()?, child.stdout.take()?);
        for fd in [stdin.as_raw_fd(), stdout.as_raw_fd()] {
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        }
        Some((child, stdin, stdout))
    }

    /// Starts Claude Code ahead of the first message (the launch is most of the first reply's wait).
    pub fn prewarm(&mut self) {
        if self.proc.is_none() {
            self.proc = self.spawn();
        }
    }

    /// Hands queued input to Claude Code as far as the pipe takes it; false once it can't take any
    /// more ever (gone).
    pub fn flush(&mut self) -> bool {
        let Some((_, stdin, _)) = &mut self.proc else { return false };
        while self.sent < self.out.len() {
            match stdin.write(&self.out[self.sent..]) {
                Ok(n) if n > 0 => self.sent += n,
                // the rest on POLLOUT
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                _ => {
                    // nothing left to poll for
                    self.out.clear();
                    self.sent = 0;
                    return false;
                }
            }
        }
        self.out.clear();
        self.sent = 0;
        true
    }

    fn write(&mut self, line: &str) -> bool {
        if self.proc.is_none() {
            return false;
        }
        self.out.extend_from_slice(line.as_bytes());
        self.out.push(b'\n');
        self.flush()
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

    /// Ctrl+M on an empty chat: a process of the other mode replaces the running one.
    pub fn set_full(&mut self, full: bool) {
        if self.full != full {
            self.full = full;
            self.reset();
        }
    }

    /// Ctrl+N: the next message starts a fresh process (and conversation).
    pub fn reset(&mut self) {
        if let Some((mut child, _, _)) = self.proc.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.buf.clear();
        self.out.clear();
        self.sent = 0;
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
        // complete lines only, and one drain for all of them
        let end = self.buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        for line in self.buf[..end].split(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(line);
            let line = line.trim();
            // only what the page renders: text/thinking deltas and the end of a turn
            if line.contains("\"text_delta\"") || line.contains("\"thinking_delta\"") || line.contains("\"type\":\"result\"") {
                lines.push(line.to_string());
            }
        }
        self.buf.drain(..end);
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
    fn partial_writes() {
        let mut child = Command::new("cat").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
        unsafe { libc::fcntl(stdin.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let mut chat = Chat::default();
        chat.proc = Some((child, stdin, stdout));
        // more than the pipe takes at once: the rest goes in over several flushes, in order
        let line: String = (0..4_000_000u32).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        assert!(chat.write(&line));
        assert!(chat.write_fd() >= 0);
        let mut got = vec![];
        let mut chunk = [0u8; 65536];
        while got.len() <= line.len() {
            assert!(chat.flush());
            let (_, _, out) = chat.proc.as_mut().unwrap();
            let n = out.read(&mut chunk).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(got, format!("{line}\n").into_bytes());
        assert_eq!((chat.write_fd(), chat.out.len(), chat.sent), (-1, 0, 0));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("a\"\\\n\u{1}\u{2028}"), r#""a\"\\\n\u0001\u2028""#);
    }
}
