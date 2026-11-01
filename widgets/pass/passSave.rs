use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::{background_section, header_style};
use crate::lib::clipboard::{copy_login, copy_sensitive};
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::process::exec;
use crate::lib::searchDialog::plain_entry;
use crate::lib::state::State;
use crate::lib::style::*;
use crate::ui::entry;
use crate::ui::node::{Align, Node, Sides};
use crate::ui::{runtime, window};

const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &str = "0123456789";
const SYMBOLS: &str = "!@#$%^&*()-_=+[]{};:,.?";

/// Uniform random bytes from one getrandom buffer, refilled as rejection sampling uses it up.
struct Rng {
    buf: [u8; 256],
    pos: usize,
}

impl Rng {
    fn new() -> Rng {
        Rng { buf: [0; 256], pos: 256 }
    }

    fn byte(&mut self) -> u8 {
        if self.pos == self.buf.len() {
            let n = unsafe { libc::getrandom(self.buf.as_mut_ptr().cast(), self.buf.len(), 0) };
            if n != self.buf.len() as isize {
                std::process::abort(); // never a password from a predictable buffer
            }
            self.pos = 0;
        }
        self.pos += 1;
        self.buf[self.pos - 1]
    }

    /// Uniform in 0..n (n <= 256): bytes past the last whole multiple of n are rejected.
    fn below(&mut self, n: usize) -> usize {
        let limit = 256 - 256 % n;
        loop {
            let b = self.byte() as usize;
            if b < limit {
                return b % n;
            }
        }
    }
}

impl Drop for Rng {
    fn drop(&mut self) {
        for b in &mut self.buf {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

pub fn random_password(length: usize) -> String {
    let alphabet = format!("{LOWER}{UPPER}{DIGITS}{SYMBOLS}");
    let mut rng = Rng::new();
    let mut pick = |set: &str| set.as_bytes()[rng.below(set.len())];
    let mut chars = vec![pick(LOWER), pick(UPPER), pick(DIGITS), pick(SYMBOLS)];
    while chars.len() < length {
        chars.push(pick(&alphabet));
    }
    for i in (1..chars.len()).rev() {
        let j = rng.below(i + 1);
        chars.swap(i, j);
    }
    String::from_utf8(chars).unwrap_or_default() // ASCII
}

thread_local! {
    static GENERATED: State<bool> = State::new(false);
}

pub fn pass_save() {
    let site = entry::new("Website");
    let user = entry::new("Username");
    let pass = entry::new("Password");
    pass.borrow_mut().password = true;

    let generate = {
        let pass = pass.clone();
        Rc::new(move || {
            let mut p = random_password(20);
            pass.borrow_mut().set_text(&p);
            GENERATED.with(|g| g.set(true));
            copy_sensitive(&p);
            crate::ui::entry::wipe(&mut p);
        })
    };

    let save = {
        let (site, user, pass) = (site.clone(), user.clone(), pass.clone());
        Rc::new(move || {
            let s = site.borrow().text.trim().to_string();
            let u = user.borrow().text.trim().to_string();
            if s.is_empty() || u.is_empty() || pass.borrow().text.is_empty() {
                return;
            }
            let mut p = pass.borrow().text.clone();
            window::set_visible("passSave", false);
            runtime::spawn(async move {
                let mut entry = format!("{p}\nlogin: {u}\n");
                let saved = crate::lib::process::run_input(&["pass", "insert", "-m", "-f", &s], entry.as_bytes()).await;
                crate::ui::entry::wipe(&mut entry);
                if !saved {
                    crate::ui::entry::wipe(&mut p);
                    crate::lib::notifySend::notify_send("Pass", "Unable to save password", "", vec![]);
                    return;
                }
                copy_login(&u, &p);
                crate::ui::entry::wipe(&mut p);
                if exec(&["pass-sync"]).await.is_err() {
                    crate::lib::notifySend::notify_send("Pass", "Pass sync failed", "", vec![]);
                }
            });
        })
    };

    let build = {
        let (site, user, pass) = (site.clone(), user.clone(), pass.clone());
        move |_: &window::View| {
            let base = window();
            let generated = GENERATED.with(|g| g.peek());
            let mut icon = base.inherit();
            icon.color = if generated { blue2() } else { dark4() };
            icon.margin = Sides::trbl(0.0, SPACE_S, 0.0, 0.0);
            let content = Node::vbox(base.inherit(), SPACE_S, vec![
                plain_entry(&base, &site, "site"),
                plain_entry(&base, &user, "user"),
                Node::hbox(base.inherit(), SPACE_S, vec![
                    plain_entry(&base, &pass, "pass"),
                    Node::icon(icon, "dialog-password-symbolic", 16.0).align(Align::Fill, Align::Center),
                ]),
            ]);
            let header = Node::label(header_style(&base), "Password");
            Node::hbox(base.inherit(), 0.0, vec![background_section(&base, header, Some(content), 300.0, 400.0)]).align(Align::Center, Align::Center)
        }
    };

    let goto = |id: &'static str| -> crate::lib::inputControl::Action {
        Rc::new(move || {
            if let Some(w) = window::get("passSave") {
                w.grab_focus(id);
            }
        })
    };
    let reset = {
        let (user, pass) = (user.clone(), pass.clone());
        Rc::new(move || {
            user.borrow_mut().set_text("");
            pass.borrow_mut().set_text("");
            GENERATED.with(|g| g.set(false));
        })
    };
    let tab = {
        let generate = generate.clone();
        Rc::new(move |ev: &crate::ui::wayland::KeyEvent| {
            if ev.sym != Keysym::Tab || ev.shift {
                return false;
            }
            generate();
            true
        })
    };
    let w = input_control("passSave", build, Opts {
        on_show: Some(reset),
        focus: Some("site"),
        entries: vec![
            EntryBinding { id: "site", entry: site.clone(), on_activate: Some(goto("user")), on_key: None },
            EntryBinding { id: "user", entry: user.clone(), on_activate: Some(goto("pass")), on_key: None },
            EntryBinding { id: "pass", entry: pass.clone(), on_activate: Some(save), on_key: Some(tab) },
        ],
        ..Default::default()
    });
    w.watch(&GENERATED.with(Clone::clone));
    // nothing typed stays behind a hidden dialog
    let v = w.visibility.clone();
    w.visibility.subscribe(move || {
        if !v.peek() {
            for e in [&site, &user, &pass] {
                e.borrow_mut().set_text("");
            }
            GENERATED.with(|g| g.set(false));
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn password_has_every_class() {
        let p = super::random_password(20);
        assert_eq!(p.len(), 20);
        for set in [super::LOWER, super::UPPER, super::DIGITS, super::SYMBOLS] {
            assert!(p.chars().any(|c| set.contains(c)));
        }
    }
}
