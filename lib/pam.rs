// PAM on a worker thread: pam_start + pam_authenticate, conversation messages
// reported back to the poll loop through a pipe.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use crate::ui::entry::Secret;
use crate::ui::runtime;

#[repr(C)]
struct PamMessage {
    style: c_int,
    msg: *const c_char,
}
#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    retcode: c_int,
}
type ConvFn = extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;
#[repr(C)]
struct PamConv {
    conv: ConvFn,
    data: *mut c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(service: *const c_char, user: *const c_char, conv: *const PamConv, h: *mut *mut c_void) -> c_int;
    fn pam_authenticate(h: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(h: *mut c_void, status: c_int) -> c_int;
}

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
const PAM_CONV_ERR: c_int = 19;

pub const PAM_AUTH_ERR: c_int = 7;
pub const PAM_MAXTRIES: c_int = 11;

pub enum Event {
    Info,
    Error,
    /// pam_authenticate's raw return code (0: success); -1 if the worker died without one
    Done(c_int),
}

/// Frames on the pipe: a kind byte, then the code (Done) or 0, little-endian.
const FRAME: usize = 5;
const DONE: u8 = 0;
const INFO: u8 = 1;
const ERROR: u8 = 2;

struct ConvData {
    /// nul-terminated
    password: Option<Secret>,
    /// the secret answers one prompt only
    answered: bool,
    out: std::fs::File,
}

fn report(out: &mut std::fs::File, kind: u8, code: c_int) {
    let mut buf = [kind, 0, 0, 0, 0];
    buf[1..].copy_from_slice(&code.to_le_bytes());
    let _ = out.write_all(&buf);
}

extern "C" fn conv(n: c_int, msgs: *mut *const PamMessage, resp: *mut *mut PamResponse, data: *mut c_void) -> c_int {
    let d = unsafe { &mut *data.cast::<ConvData>() };
    let n = n.max(0) as usize;
    let out = unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) }.cast::<PamResponse>();
    if out.is_null() {
        return 5; // PAM_BUF_ERR
    }
    for i in 0..n {
        let m = unsafe { &**msgs.add(i) };
        let r = unsafe { &mut *out.add(i) };
        match m.style {
            PAM_PROMPT_ECHO_OFF if !d.answered => {
                // the secret, or an empty answer when it held a NUL
                d.answered = true;
                let s = d.password.as_deref().map(|p| p.as_ptr().cast()).unwrap_or(c"".as_ptr());
                r.resp = unsafe { libc::strdup(s) };
            }
            PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                // a second secret or a visible answer (username, OTP...): never the password
                for j in 0..i {
                    let r = unsafe { &mut *out.add(j) };
                    if !r.resp.is_null() {
                        unsafe {
                            let len = libc::strlen(r.resp);
                            std::ptr::write_bytes(r.resp, 0, len);
                            libc::free(r.resp.cast());
                        }
                    }
                }
                unsafe { libc::free(out.cast()) };
                return PAM_CONV_ERR;
            }
            PAM_ERROR_MSG => report(&mut d.out, ERROR, 0),
            PAM_TEXT_INFO => report(&mut d.out, INFO, 0),
            _ => {}
        }
    }
    unsafe { *resp = out };
    0
}

thread_local! {
    /// looked up once, on the loop thread (getpwuid isn't thread-safe)
    static USER: CString = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() { CString::default() } else { CStr::from_ptr((*pw).pw_name).to_owned() }
    };
}

/// The password nul-terminated in a buffer that wipes itself, leaving no unwiped copy behind.
fn c_password(p: Secret) -> Option<Secret> {
    if p.contains(&0) {
        return None;
    }
    let mut c = Vec::with_capacity(p.len() + 1);
    c.extend_from_slice(&p);
    c.push(0);
    Some(Secret::new(c))
}

/// Runs PAM `service` for the current user; `on_event` receives conversation messages and the result.
pub fn authenticate(service: &str, password: Secret, on_event: impl Fn(Event) + 'static) {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        on_event(Event::Done(-1));
        return;
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let service = CString::new(service).unwrap_or_default();
    let password = c_password(password);
    let user = USER.with(Clone::clone);
    let work = move || {
        let mut data = ConvData { password, answered: false, out: std::fs::File::from(wr) };
        let c = PamConv { conv, data: (&raw mut data).cast() };
        let mut h = std::ptr::null_mut();
        let mut ret = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &c, &mut h) };
        if ret == 0 {
            ret = unsafe { pam_authenticate(h, 0) };
            unsafe { pam_end(h, ret) };
        }
        report(&mut data.out, DONE, ret);
        // dropping `data` wipes the password
    };
    // shared so a refused spawn (thread limit) hands the job back to run here
    let job = Arc::new(Mutex::new(Some(work)));
    let theirs = job.clone();
    if let Err(e) = std::thread::Builder::new().spawn(move || {
        if let Some(w) = theirs.lock().ok().and_then(|mut j| j.take()) {
            w();
        }
    }) {
        eprintln!("flurry: thread spawn failed, running PAM inline: {e}");
        if let Some(w) = job.lock().ok().and_then(|mut j| j.take()) {
            w();
        }
    }
    let raw = rd.as_raw_fd();
    let file = std::fs::File::from(rd);
    let buf = RefCell::new(Vec::<u8>::new());
    let done = Cell::new(false);
    runtime::watch(raw, move || {
        let mut chunk = [0u8; 1024];
        // only called when readable: an error reads as the end, which fails the attempt
        let n = (&file).read(&mut chunk).unwrap_or(0);
        if n == 0 {
            runtime::unwatch(raw);
            // the worker went away without a result: never leave the caller waiting
            if !done.replace(true) {
                on_event(Event::Done(-1));
            }
            return;
        }
        let mut b = buf.borrow_mut();
        b.extend_from_slice(&chunk[..n]);
        drain_frames(&mut b, |ev| {
            if matches!(ev, Event::Done(_)) {
                done.set(true);
            }
            on_event(ev);
        });
    });
}

/// Hands every whole frame in `buf` to `f`, keeping a trailing partial one for the next read.
fn drain_frames(buf: &mut Vec<u8>, mut f: impl FnMut(Event)) {
    let frames = buf.len() / FRAME * FRAME;
    for fr in buf[..frames].as_chunks::<FRAME>().0 {
        let code = c_int::from_le_bytes([fr[1], fr[2], fr[3], fr[4]]);
        f(match fr[0] {
            DONE => Event::Done(code),
            INFO => Event::Info,
            _ => Event::Error,
        });
    }
    buf.drain(..frames);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(s: &[u8]) -> Secret {
        Secret::new(s.to_vec())
    }

    #[test]
    fn password_is_nul_terminated() {
        assert_eq!(&*c_password(secret(b"pw")).unwrap(), b"pw\0");
        assert!(c_password(secret(b"a\0b")).is_none());
    }

    fn written(frames: &[(u8, c_int)]) -> Vec<u8> {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut out = std::fs::File::from(wr);
        for &(kind, code) in frames {
            report(&mut out, kind, code);
        }
        drop(out);
        let mut buf = vec![];
        std::fs::File::from(rd).read_to_end(&mut buf).unwrap();
        buf
    }

    fn label(ev: Event) -> (char, c_int) {
        match ev {
            Event::Info => ('i', 0),
            Event::Error => ('e', 0),
            Event::Done(c) => ('d', c),
        }
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = written(&[(INFO, 0), (ERROR, 0), (DONE, PAM_AUTH_ERR), (DONE, -1)]);
        assert_eq!(buf.len(), 4 * FRAME);
        let mut got = vec![];
        drain_frames(&mut buf, |ev| got.push(label(ev)));
        assert_eq!(got, [('i', 0), ('e', 0), ('d', PAM_AUTH_ERR), ('d', -1)]);
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_frame_waits_for_the_rest() {
        let all = written(&[(INFO, 0), (DONE, 0)]);
        let mut buf = all[..FRAME + 2].to_vec();
        let mut got = vec![];
        drain_frames(&mut buf, |ev| got.push(label(ev)));
        assert_eq!(got, [('i', 0)]);
        assert_eq!(buf, all[FRAME..FRAME + 2]);
        buf.extend_from_slice(&all[FRAME + 2..]);
        drain_frames(&mut buf, |ev| got.push(label(ev)));
        assert_eq!(got, [('i', 0), ('d', 0)]);
        assert!(buf.is_empty());
    }
}
