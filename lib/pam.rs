// PAM on a worker thread: pam_start + pam_authenticate, conversation messages
// reported back to the poll loop through a pipe.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;

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

struct ConvData {
    password: Option<CString>,
    /// the secret answers one prompt only
    answered: bool,
    out: std::fs::File,
}

fn report(out: &mut std::fs::File, kind: u8, text: &str) {
    let mut buf = vec![kind];
    buf.extend_from_slice(&(text.len() as u32).to_le_bytes());
    buf.extend_from_slice(text.as_bytes());
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
        let text = if m.msg.is_null() { String::new() } else { unsafe { CStr::from_ptr(m.msg) }.to_string_lossy().into_owned() };
        let r = unsafe { &mut *out.add(i) };
        match m.style {
            PAM_PROMPT_ECHO_OFF if !d.answered => {
                // the secret, or an empty answer when there is none (fprint skip)
                d.answered = true;
                let s = d.password.as_ref().map(|p| p.as_ptr()).unwrap_or(c"".as_ptr());
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
            PAM_ERROR_MSG => report(&mut d.out, 2, &text),
            PAM_TEXT_INFO => report(&mut d.out, 1, &text),
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

fn wipe_bytes(v: &mut Vec<u8>) {
    let p = v.as_mut_ptr();
    for i in 0..v.capacity() {
        unsafe { std::ptr::write_volatile(p.add(i), 0) };
    }
    v.clear();
}

/// The password as a C string without leaving unwiped copies behind (no realloc for the nul).
fn c_password(p: String) -> Option<CString> {
    let mut v = p.into_bytes();
    if v.contains(&0) {
        wipe_bytes(&mut v);
        return None;
    }
    if v.capacity() == v.len() {
        let mut bigger = Vec::with_capacity(v.len() + 1);
        bigger.extend_from_slice(&v);
        wipe_bytes(&mut v);
        v = bigger;
    }
    Some(unsafe { CString::from_vec_unchecked(v) })
}

/// Runs PAM `service` for the current user; `on_event` receives conversation messages and the result.
pub fn authenticate(service: &str, password: Option<String>, on_event: impl Fn(Event) + 'static) {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        on_event(Event::Done(-1));
        return;
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let service = CString::new(service).unwrap_or_default();
    let password = password.and_then(c_password);
    let user = USER.with(Clone::clone);
    std::thread::spawn(move || {
        let out = std::fs::File::from(wr);
        let mut data = ConvData { password, answered: false, out };
        let c = PamConv { conv, data: (&raw mut data).cast() };
        let mut h = std::ptr::null_mut();
        let mut ret = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &c, &mut h) };
        if ret == 0 {
            ret = unsafe { pam_authenticate(h, 0) };
            unsafe { pam_end(h, ret) };
        }
        if let Some(p) = data.password.take() {
            wipe_bytes(&mut p.into_bytes_with_nul());
        }
        let mut out = data.out;
        report(&mut out, 0, &ret.to_string());
    });
    let raw = rd.as_raw_fd();
    let file = std::cell::RefCell::new(std::fs::File::from(rd));
    let on_event = Rc::new(on_event);
    let buf = std::cell::RefCell::new(Vec::<u8>::new());
    let done = std::cell::Cell::new(false);
    runtime::watch(raw, move || {
        let mut chunk = [0u8; 1024];
        // a signal mid-read: retry a few times, then leave it to the next poll (still readable)
        let mut read = None;
        for _ in 0..4 {
            match file.borrow_mut().read(&mut chunk) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                r => read = Some(r.unwrap_or(0)),
            }
            break;
        }
        let Some(n) = read else { return };
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
        while b.len() >= 5 {
            let len = u32::from_le_bytes(b[1..5].try_into().unwrap()) as usize;
            if b.len() < 5 + len {
                break;
            }
            let kind = b[0];
            let text = String::from_utf8_lossy(&b[5..5 + len]).into_owned();
            b.drain(..5 + len);
            if kind == 0 {
                done.set(true);
            }
            on_event(match kind {
                0 => Event::Done(text.parse().unwrap_or(-1)),
                1 => Event::Info,
                _ => Event::Error,
            });
        }
    });
}
