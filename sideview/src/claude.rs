// The `flurry:claude` page: a local chat (claude/index.html) backed by Claude Code in print mode on
// the user's subscription. The page talks to us through console messages (\u{1}claude:<json>,
// \u{1}claude:new, \u{1}open:<url>); stream-json events go back via window.__claude(<event>).

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const URL: &str = "flurry:claude";

const SYSTEM: &str = "Write math in LaTeX: $...$ inline, $$...$$ for display.";

macro_rules! assets {
    ($($name:literal),* $(,)?) => { &[$(($name, include_bytes!(concat!("../claude/", $name)))),*] };
}
const ASSETS: &[(&str, &[u8])] = assets![
    "index.html", "katex.min.js", "katex.min.css", "marked.min.js",
    "fonts/KaTeX_AMS-Regular.woff2", "fonts/KaTeX_Caligraphic-Bold.woff2", "fonts/KaTeX_Caligraphic-Regular.woff2",
    "fonts/KaTeX_Fraktur-Bold.woff2", "fonts/KaTeX_Fraktur-Regular.woff2", "fonts/KaTeX_Main-Bold.woff2",
    "fonts/KaTeX_Main-BoldItalic.woff2", "fonts/KaTeX_Main-Italic.woff2", "fonts/KaTeX_Main-Regular.woff2",
    "fonts/KaTeX_Math-BoldItalic.woff2", "fonts/KaTeX_Math-Italic.woff2", "fonts/KaTeX_SansSerif-Bold.woff2",
    "fonts/KaTeX_SansSerif-Italic.woff2", "fonts/KaTeX_SansSerif-Regular.woff2", "fonts/KaTeX_Script-Regular.woff2",
    "fonts/KaTeX_Size1-Regular.woff2", "fonts/KaTeX_Size2-Regular.woff2", "fonts/KaTeX_Size3-Regular.woff2",
    "fonts/KaTeX_Size4-Regular.woff2", "fonts/KaTeX_Typewriter-Regular.woff2",
];

fn dir() -> std::path::PathBuf {
    let run = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    run.join("flurry-claude")
}

/// Writes the bundled page to the runtime dir (tmpfs) and returns its file URL.
pub fn page_url() -> Option<url::Url> {
    let d = dir();
    for (name, bytes) in ASSETS {
        let p = d.join(name);
        std::fs::create_dir_all(p.parent()?).ok()?;
        std::fs::write(&p, bytes).ok()?;
    }
    url::Url::from_file_path(d.join("index.html")).ok()
}

#[derive(Default)]
pub struct Chat {
    proc: Option<(Child, ChildStdin, ChildStdout)>,
    buf: Vec<u8>,
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
        let cwd = dir();
        bins.iter().find_map(|bin| {
            let mut child = Command::new(bin)
                .args(["-p", "--safe-mode", "--model", "opus", "--effort", "medium", "--tools", "", "--no-session-persistence"])
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

    /// One stream-json user message; the conversation lives in the running process.
    pub fn send(&mut self, line: &str) -> bool {
        self.prewarm();
        let Some((_, stdin, _)) = &mut self.proc else { return false };
        stdin.write_all(line.as_bytes()).and_then(|_| stdin.write_all(b"\n")).and_then(|_| stdin.flush()).is_ok()
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
