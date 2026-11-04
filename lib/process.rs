// Child processes on the poll loop: async exec, stdin piping,
// line-streamed stdout/stderr, signals. std spawns through posix_spawn (vfork), the cheapest path,
// except with pre_exec (spawn_lines), which forces fork+exec.

use std::cell::{Cell, RefCell};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;

use crate::ui::entry::{Secret, wipe_bytes};
use crate::ui::runtime;

/// Reads per wakeup: a chatty child can't hog the loop.
const READS_PER_WAKE: usize = 16;
/// A longer line is passed on in pieces.
const MAX_LINE: usize = 64 << 10;

fn zero(b: &mut [u8]) {
    for x in b {
        unsafe { std::ptr::write_volatile(x, 0) };
    }
}

/// Spawns with a pidfd (Linux 5.3+) to wait on; without one the child is killed and reaped.
fn spawn(c: &mut Command) -> Option<(Child, OwnedFd)> {
    let mut child = c.spawn().ok()?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) } as i32;
    if fd < 0 {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    Some((child, unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// A child's output pipe; the sink gets each chunk, then one empty chunk at the end.
struct Pipe {
    fd: OwnedFd,
    sink: Box<dyn FnMut(&[u8])>,
}

type PipeRef = Rc<RefCell<Option<Pipe>>>;

fn pipe(fd: OwnedFd, sink: impl FnMut(&[u8]) + 'static) -> PipeRef {
    let raw = fd.as_raw_fd();
    runtime::set_nonblocking(raw, true);
    let p: PipeRef = Rc::new(RefCell::new(Some(Pipe { fd, sink: Box::new(sink) })));
    let w = p.clone();
    runtime::watch(raw, move || drain(&w, READS_PER_WAKE));
    p
}

/// Reads up to `max` chunks; closes the pipe at EOF, or when `max` is unbounded (the child is gone:
/// output a grandchild writes later is dropped).
fn drain(p: &PipeRef, max: usize) {
    let mut buf = [0u8; 8192];
    let mut eof = max == usize::MAX;
    if let Some(pipe) = &mut *p.borrow_mut() {
        for _ in 0..max {
            let n = unsafe { libc::read(pipe.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                (pipe.sink)(&buf[..n as usize]);
                zero(&mut buf[..n as usize]);
                continue;
            }
            let err = std::io::Error::last_os_error().kind();
            if n < 0 && err == std::io::ErrorKind::Interrupted {
                continue;
            }
            if n == 0 || err != std::io::ErrorKind::WouldBlock {
                eof = true;
            }
            break;
        }
    }
    if eof && let Some(mut pipe) = p.borrow_mut().take() {
        runtime::unwatch(pipe.fd.as_raw_fd());
        (pipe.sink)(&[]);
    }
}

/// Once `child` exits: drains its pipes to the end, then calls `exit` with the code (-1 if killed
/// by a signal).
fn supervise(child: Child, pidfd: Rc<OwnedFd>, pipes: Vec<PipeRef>, exit: impl FnOnce(i32) + 'static) {
    let raw = pidfd.as_raw_fd();
    let state = RefCell::new(Some((child, pipes, exit)));
    runtime::watch(raw, move || {
        runtime::unwatch(raw);
        let _keep = &pidfd;
        if let Some((mut c, pipes, exit)) = state.borrow_mut().take() {
            for p in &pipes {
                drain(p, usize::MAX);
            }
            exit(c.wait().ok().and_then(|s| s.code()).unwrap_or(-1));
        }
    });
}

fn command(argv: &[&str]) -> Option<Command> {
    let (prog, args) = argv.split_first()?;
    let mut c = Command::new(prog);
    c.args(args);
    Some(c)
}

/// Appends without leaving a copy behind: a growing Vec would free its old buffer unwiped.
fn append(v: &mut Vec<u8>, b: &[u8]) {
    if v.capacity() - v.len() < b.len() {
        let mut n = Vec::with_capacity((v.len() + b.len()).next_power_of_two().max(8192));
        n.extend_from_slice(v);
        wipe_bytes(v);
        *v = n;
    }
    v.extend_from_slice(b);
}

/// No copy for valid UTF-8; a lossy one wipes the original.
fn into_string(v: Vec<u8>) -> String {
    String::from_utf8(v).unwrap_or_else(|e| {
        let mut v = e.into_bytes();
        let s = String::from_utf8_lossy(&v).into_owned();
        wipe_bytes(&mut v);
        s
    })
}

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs to completion, collecting output. Nothing is left unwiped behind the returned strings.
pub async fn output(argv: &[&str], stdin: Option<&[u8]>) -> Option<Output> {
    let mut c = command(argv)?;
    c.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut child, pidfd) = spawn(&mut c)?;
    if let (Some(data), Some(si)) = (stdin, child.stdin.take()) {
        write_stdin(si.into(), data);
    }
    let out = Rc::new(RefCell::new((Vec::new(), Vec::new())));
    let (o, e) = (out.clone(), out.clone());
    let pipes = vec![
        pipe(child.stdout.take()?.into(), move |b| append(&mut o.borrow_mut().0, b)),
        pipe(child.stderr.take()?.into(), move |b| append(&mut e.borrow_mut().1, b)),
    ];
    let (tx, rx) = runtime::oneshot();
    supervise(child, Rc::new(pidfd), pipes, move |code| tx.send(code));
    let code = rx.await.unwrap_or(-1);
    let (so, se) = std::mem::take(&mut *out.borrow_mut());
    Some(Output { code, stdout: into_string(so), stderr: into_string(se) })
}

/// execAsync: stdout (trailing newline trimmed) on success, stderr on failure.
pub async fn exec(argv: &[&str]) -> Result<String, String> {
    match output(argv, None).await {
        Some(mut o) if o.code == 0 => {
            o.stdout.truncate(o.stdout.trim_end_matches('\n').len());
            Ok(o.stdout)
        }
        Some(mut o) => {
            o.stderr.truncate(o.stderr.trim_end().len());
            Err(o.stderr)
        }
        None => Err(format!("failed to spawn {}", argv.first().unwrap_or(&""))),
    }
}

/// Writes `data` without blocking the loop: what fits in the pipe now, the rest as it drains.
fn write_stdin(fd: OwnedFd, data: &[u8]) {
    let raw = fd.as_raw_fd();
    runtime::set_nonblocking(raw, true);
    // Some(n): n bytes written; None: done or failed
    let write = move |data: &[u8], mut n: usize| -> Option<usize> {
        while n < data.len() {
            let k = unsafe { libc::write(raw, data[n..].as_ptr().cast(), data.len() - n) };
            if k > 0 {
                n += k as usize;
                continue;
            }
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::Interrupted => {}
                std::io::ErrorKind::WouldBlock => return Some(n),
                _ => return None,
            }
        }
        None
    };
    let Some(n) = write(data, 0) else { return };
    let rest = Secret::new(data[n..].to_vec());
    let off = Cell::new(0);
    runtime::watch(raw, move || {
        let _keep = &fd;
        match write(&rest, off.get()) {
            Some(n) => {
                off.set(n);
                runtime::poll_out(raw);
            }
            None => runtime::unwatch(raw),
        }
    });
    runtime::poll_out(raw);
}

/// Exit code (-1 if killed by a signal); None if it couldn't start. No stdio.
pub async fn status(argv: &[&str]) -> Option<i32> {
    let mut c = command(argv)?;
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let (child, pidfd) = spawn(&mut c)?;
    let (tx, rx) = runtime::oneshot();
    supervise(child, Rc::new(pidfd), vec![], move |code| tx.send(code));
    Some(rx.await.unwrap_or(-1))
}

/// A running process with line-streamed output.
pub struct Proc {
    pidfd: Rc<OwnedFd>,
    exited: Rc<Cell<bool>>,
}

impl Proc {
    /// No-op once the child exited (the pidfd can't hit a reused pid).
    pub fn signal(&self, sig: i32) {
        if !self.exited.get() {
            unsafe { libc::syscall(libc::SYS_pidfd_send_signal, self.pidfd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0) };
        }
    }

    /// True from just before its exit callback runs.
    pub fn exited(&self) -> bool {
        self.exited.get()
    }
}

fn lines(cb: impl Fn(&str) + 'static) -> impl FnMut(&[u8]) + 'static {
    let mut pending = Vec::<u8>::new();
    move |b: &[u8]| {
        if b.is_empty() {
            if !pending.is_empty() {
                cb(&String::from_utf8_lossy(&pending));
                pending.clear();
            }
            return;
        }
        let mut from = pending.len();
        pending.extend_from_slice(b);
        let mut start = 0;
        while let Some(i) = pending[from..].iter().position(|&c| c == b'\n') {
            cb(&String::from_utf8_lossy(&pending[start..from + i]));
            start = from + i + 1;
            from = start;
        }
        pending.drain(..start);
        if pending.len() > MAX_LINE {
            cb(&String::from_utf8_lossy(&pending));
            pending.clear();
        }
    }
}

pub fn spawn_lines(
    argv: &[&str], stdout: impl Fn(&str) + 'static, stderr: impl Fn(&str) + 'static, exit: impl FnOnce(i32) + 'static,
) -> Option<Proc> {
    let mut c = command(argv)?;
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // long-lived helpers (gpu-screen-recorder, ffmpeg) must not outlive the shell; a shell that
    // died before prctl ran would never deliver the signal, so the child checks it's still ours
    let parent = unsafe { libc::getpid() };
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut c, move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            if libc::getppid() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    let (mut child, pidfd) = spawn(&mut c)?;
    let pidfd = Rc::new(pidfd);
    let pipes = vec![pipe(child.stdout.take()?.into(), lines(stdout)), pipe(child.stderr.take()?.into(), lines(stderr))];
    let exited = Rc::new(Cell::new(false));
    let e = exited.clone();
    supervise(child, pidfd.clone(), pipes, move |code| {
        e.set(true);
        exit(code)
    });
    Some(Proc { pidfd, exited })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(done: impl Fn() -> bool) {
        for _ in 0..200 {
            runtime::iterate(false);
            if done() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn exec_collects_stdout_and_status() {
        let got = Rc::new(RefCell::new(None));
        let g = got.clone();
        runtime::spawn(async move {
            *g.borrow_mut() = Some((exec(&["sh", "-c", "echo hi"]).await, exec(&["sh", "-c", "echo bad >&2; exit 3"]).await));
        });
        drive(|| got.borrow().is_some());
        let (a, b) = got.borrow_mut().take().expect("finished");
        assert_eq!(a, Ok("hi".into()));
        assert_eq!(b, Err("bad".into()));
    }

    #[test]
    fn lines_flush_before_exit() {
        let got = Rc::new(RefCell::new(Vec::<String>::new()));
        let (g, g2, g3) = (got.clone(), got.clone(), got.clone());
        let _p = spawn_lines(
            &["sh", "-c", "printf 'a\\nb'; printf 'err' >&2; exit 2"],
            move |l| g.borrow_mut().push(l.into()),
            move |l| g2.borrow_mut().push(format!("!{l}")),
            move |code| g3.borrow_mut().push(code.to_string()),
        );
        drive(|| got.borrow().last().is_some_and(|l| l == "2"));
        // the pipes may hit EOF before or after the exit wakeup, in either order
        let mut out = got.borrow().clone();
        assert_eq!(out.pop().as_deref(), Some("2"));
        out.sort();
        assert_eq!(out, ["!err", "a", "b"]);
    }

    #[test]
    fn stdin_larger_than_the_pipe() {
        let data = vec![b'x'; 300_000];
        let got = Rc::new(RefCell::new(None));
        let g = got.clone();
        runtime::spawn(async move {
            *g.borrow_mut() = Some(output(&["wc", "-c"], Some(&data)).await.map(|o| o.stdout));
        });
        drive(|| got.borrow().is_some());
        assert_eq!(got.borrow_mut().take().flatten().map(|s| s.trim().to_string()), Some("300000".into()));
    }
}
