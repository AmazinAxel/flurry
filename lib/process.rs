// Child processes on the poll loop: async exec, stdin piping,
// line-streamed stdout/stderr, signals. std spawns through posix_spawn (vfork), the cheapest path,
// except with pre_exec (spawn_lines), which forces fork+exec.

use std::cell::{Cell, RefCell};
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
fn wait(child: Child) -> runtime::Receiver<i32> {
    let fd = pidfd(child.id()).map(Rc::new);
    wait_fd(child, fd)
}

fn wait_fd(mut child: Child, fd: Option<Rc<OwnedFd>>) -> runtime::Receiver<i32> {
    let (tx, rx) = runtime::oneshot();
    let Some(fd) = fd else {
        // no pidfd (old kernel): wait on a thread so the loop never blocks
        runtime::spawn(async move {
            let code = runtime::blocking(move || child.wait().ok().and_then(|s| s.code()).unwrap_or(-1)).await;
            tx.send(code.unwrap_or(-1));
        });
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
    if let (Some(data), Some(si)) = (stdin, child.stdin.take()) {
        write_stdin(si, data);
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

/// Writes `data` without blocking the loop: what fits in the pipe now, the rest from a thread.
fn write_stdin(mut si: std::process::ChildStdin, data: &[u8]) {
    let raw = si.as_raw_fd();
    nonblock(raw);
    let mut n = 0;
    while n < data.len() {
        match si.write(&data[n..]) {
            Ok(0) => return,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return,
        }
    }
    if n < data.len() {
        unsafe { libc::fcntl(raw, libc::F_SETFL, libc::fcntl(raw, libc::F_GETFL) & !libc::O_NONBLOCK) };
        let rest = data[n..].to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&rest);
        });
    }
}

/// Exit code (-1 if killed by a signal); None if it couldn't start. No stdio.
pub async fn status(argv: &[&str]) -> Option<i32> {
    let mut c = command(argv)?;
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = c.spawn().ok()?;
    Some(wait(child).await.unwrap_or(-1))
}

/// Pipes `input` to stdin; true on exit status 0.
pub async fn run_input(argv: &[&str], input: &[u8]) -> bool {
    matches!(output(argv, Some(input)).await, Some(o) if o.code == 0)
}


/// A running process with line-streamed output.
pub struct Proc {
    pub pid: u32,
    pidfd: Option<Rc<OwnedFd>>,
    exited: Rc<Cell<bool>>,
}

impl Proc {
    /// No-op once the child exited (the pidfd can't hit a reused pid).
    pub fn signal(&self, sig: i32) {
        if self.exited.get() {
            return;
        }
        match &self.pidfd {
            Some(fd) => unsafe {
                libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0);
            },
            None => unsafe {
                libc::kill(self.pid as libc::pid_t, sig);
            },
        }
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
    let pidfd = pidfd(pid).map(Rc::new);
    let exited = Rc::new(Cell::new(false));
    read_stream(child.stdout.take()?.into(), lines(stdout), || {});
    read_stream(child.stderr.take()?.into(), lines(stderr), || {});
    let rx = wait_fd(child, pidfd.clone());
    let e = exited.clone();
    runtime::spawn(async move {
        let code = rx.await.unwrap_or(-1);
        e.set(true);
        exit(code)
    });
    Some(Proc { pid, pidfd, exited })
}

#[cfg(test)]
mod tests {
    use super::*;

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
