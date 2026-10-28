// Child processes on the poll loop (ags/process + AstalIO.Process): async exec, stdin piping,
// line-streamed stdout/stderr, signals. std spawns through posix_spawn (vfork), the cheapest path.

use std::cell::RefCell;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;

use crate::ui::runtime;

fn nonblock(fd: i32) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
}

fn pidfd(pid: u32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as i32;
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Reads `fd` until EOF, calling `chunk` with each piece and `done` at EOF.
fn read_stream(fd: OwnedFd, chunk: impl Fn(&[u8]) + 'static, done: impl FnOnce() + 'static) {
    let raw = fd.as_raw_fd();
    nonblock(raw);
    let done = RefCell::new(Some(done));
    runtime::watch(raw, move || {
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                chunk(&buf[..n as usize]);
                continue;
            }
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                return;
            }
            runtime::unwatch(raw);
            let _keep = &fd;
            if let Some(d) = done.borrow_mut().take() {
                d();
            }
            return;
        }
    });
}

/// Resolves to the exit code once `child` exits (-1 if killed by a signal).
fn wait(mut child: Child) -> runtime::Receiver<i32> {
    let (tx, rx) = runtime::oneshot();
    let Some(fd) = pidfd(child.id()) else {
        tx.send(child.wait().ok().and_then(|s| s.code()).unwrap_or(-1));
        return rx;
    };
    let raw = fd.as_raw_fd();
    let child = RefCell::new(Some(child));
    runtime::watch(raw, move || {
        runtime::unwatch(raw);
        let _keep = &fd;
        if let Some(mut c) = child.borrow_mut().take() {
            tx.send(c.wait().ok().and_then(|s| s.code()).unwrap_or(-1));
        }
    });
    rx
}

fn command(argv: &[&str]) -> Option<Command> {
    let (prog, args) = argv.split_first()?;
    let mut c = Command::new(prog);
    c.args(args);
    Some(c)
}

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs to completion, collecting output.
pub async fn output(argv: &[&str], stdin: Option<&[u8]>) -> Option<Output> {
    let mut c = command(argv)?;
    c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().ok()?;
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        // inputs are small (passwords, clipboard text); a blocking write is fine
        let _ = si.write_all(data);
    }
    let out = Rc::new(RefCell::new((Vec::new(), Vec::new())));
    let (tx_o, rx_o) = runtime::oneshot::<()>();
    let (tx_e, rx_e) = runtime::oneshot::<()>();
    let o = out.clone();
    read_stream(child.stdout.take()?.into(), move |b| o.borrow_mut().0.extend_from_slice(b), move || tx_o.send(()));
    let o = out.clone();
    read_stream(child.stderr.take()?.into(), move |b| o.borrow_mut().1.extend_from_slice(b), move || tx_e.send(()));
    let code = wait(child).await.unwrap_or(-1);
    rx_o.await;
    rx_e.await;
    let (so, se) = std::mem::take(&mut *out.borrow_mut());
    Some(Output { code, stdout: String::from_utf8_lossy(&so).into_owned(), stderr: String::from_utf8_lossy(&se).into_owned() })
}

/// execAsync: stdout (trailing newline trimmed) on success, stderr on failure.
pub async fn exec(argv: &[&str]) -> Result<String, String> {
    match output(argv, None).await {
        Some(o) if o.code == 0 => Ok(o.stdout.trim_end_matches('\n').to_string()),
        Some(o) => Err(o.stderr.trim_end().to_string()),
        None => Err(format!("failed to spawn {}", argv.first().unwrap_or(&""))),
    }
}

/// execAsync with a command string, split like GLib's g_shell_parse_argv.
pub async fn exec_str(cmd: &str) -> Result<String, String> {
    let argv = shell_split(cmd);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    exec(&refs).await
}

/// Pipes `input` to stdin; true on exit status 0.
pub async fn run_input(argv: &[&str], input: &[u8]) -> bool {
    matches!(output(argv, Some(input)).await, Some(o) if o.code == 0)
}


/// A running process with line-streamed output (AstalIO.Process).
pub struct Proc {
    pub pid: u32,
}

impl Proc {
    pub fn signal(&self, sig: i32) {
        unsafe { libc::kill(self.pid as libc::pid_t, sig) };
    }
}

fn lines(cb: impl Fn(&str) + 'static) -> impl Fn(&[u8]) + 'static {
    let pending = RefCell::new(Vec::<u8>::new());
    move |b: &[u8]| {
        let mut p = pending.borrow_mut();
        p.extend_from_slice(b);
        while let Some(i) = p.iter().position(|&c| c == b'\n') {
            let line: Vec<u8> = p.drain(..=i).collect();
            cb(String::from_utf8_lossy(&line[..line.len() - 1]).as_ref());
        }
    }
}

pub fn spawn_lines(
    argv: &[&str], stdout: impl Fn(&str) + 'static, stderr: impl Fn(&str) + 'static, exit: impl FnOnce(i32) + 'static,
) -> Option<Proc> {
    let mut c = command(argv)?;
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // long-lived helpers (gpu-screen-recorder, ffmpeg) must not outlive the shell
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut c, || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = c.spawn().ok()?;
    let pid = child.id();
    read_stream(child.stdout.take()?.into(), lines(stdout), || {});
    read_stream(child.stderr.take()?.into(), lines(stderr), || {});
    let rx = wait(child);
    runtime::spawn(async move { exit(rx.await.unwrap_or(-1)) });
    Some(Proc { pid })
}

/// g_shell_parse_argv: whitespace splits, single quotes literal, double quotes with backslash escapes.
pub fn shell_split(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut has = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                has = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                has = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => cur.push(chars.next().unwrap()),
                        _ => cur.push(c),
                    }
                }
            }
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    has = true;
                }
            }
            c if c.is_whitespace() => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// Runs with stdout written to `path` (binary-safe); true on exit status 0.
pub async fn to_file(argv: &[&str], path: &str) -> bool {
    let Some(mut c) = command(argv) else { return false };
    let Ok(f) = std::fs::File::create(path) else { return false };
    c.stdin(Stdio::null()).stdout(f).stderr(Stdio::null());
    match c.spawn() {
        Ok(child) => wait(child).await == Some(0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_glib() {
        assert_eq!(
            shell_split(r#"footclient -e fish -c 'sys-sync; echo "Press a key"; read'"#),
            vec!["footclient", "-e", "fish", "-c", r#"sys-sync; echo "Press a key"; read"#]
        );
        assert_eq!(shell_split(r#"nemo "/home/x y/Downloads""#), vec!["nemo", "/home/x y/Downloads"]);
    }

    #[test]
    fn exec_collects_stdout_and_status() {
        let got = Rc::new(RefCell::new(None));
        let g = got.clone();
        runtime::spawn(async move {
            *g.borrow_mut() = Some((exec(&["sh", "-c", "echo hi"]).await, exec(&["sh", "-c", "echo bad >&2; exit 3"]).await));
        });
        for _ in 0..200 {
            runtime::iterate(false);
            if got.borrow().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (a, b) = got.borrow_mut().take().expect("finished");
        assert_eq!(a, Ok("hi".into()));
        assert_eq!(b, Err("bad".into()));
    }
}
