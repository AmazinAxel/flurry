use std::cell::{Cell, RefCell};
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::{background_section, header_style};
use crate::lib::clipboard::{copy_login, copy_sensitive};
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::notifySend::notify_send;
use crate::lib::process::{exec, output};
use crate::lib::random::Rng;
use crate::lib::searchDialog::plain_entry;
use crate::lib::style::*;
use crate::ui::entry::{self, wipe};
use crate::ui::node::{Align, Node, Sides};
use crate::ui::state::global_state;
use crate::ui::{runtime, window};

const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &str = "0123456789";
const SYMBOLS: &str = "!@#$%^&*()-_=+[]{};:,.?";

/// One of each class, then the whole alphabet, shuffled; None if the kernel gives no randomness.
pub fn random_password(length: usize) -> Option<String> {
    let alphabet = format!("{LOWER}{UPPER}{DIGITS}{SYMBOLS}");
    let mut rng = Rng::default();
    // sized up front: no reallocation leaves a copy behind
    let mut chars = Vec::with_capacity(length.max(4));
    let ok = (|| {
        for set in [LOWER, UPPER, DIGITS, SYMBOLS] {
            chars.push(set.as_bytes()[rng.below(set.len())?]);
        }
        while chars.len() < length {
            chars.push(alphabet.as_bytes()[rng.below(alphabet.len())?]);
        }
        for i in (1..chars.len()).rev() {
            let j = rng.below(i + 1)?;
            chars.swap(i, j);
        }
        Some(())
    })();
    if ok.is_none() {
        entry::wipe_bytes(&mut chars);
        return None;
    }
    String::from_utf8(chars).ok() // ASCII
}

global_state! {
    fn generated() -> bool = false;
}

thread_local! {
    /// a save is running: the dialog stays up, a second Enter does nothing
    static SAVING: Cell<bool> = const { Cell::new(false) };
    /// the existing entry an Enter was refused for: the next Enter on it overwrites
    static CONFIRM: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The settings' sync argv, when its program is installed.
async fn sync() {
    let argv = crate::settings::PASS_SYNC_COMMAND;
    if argv.first().is_some_and(|p| crate::lib::apps::in_path(p)) && exec(argv).await.is_err() {
        notify_send("Pass", "Pass sync failed", "", vec![]);
    }
}

pub fn pass_save() {
    let site = entry::new("Website");
    let user = entry::new("Username");
    let pass = entry::new("Password");
    pass.borrow_mut().password = true;

    let generate = {
        let pass = pass.clone();
        Rc::new(move || {
            let Some(mut p) = random_password(20) else {
                notify_send("Pass", "No randomness for a password", "", vec![]);
                return;
            };
            pass.borrow_mut().set_text(&p);
            generated().set(true);
            copy_sensitive(&p);
            wipe(&mut p);
        })
    };

    let save = {
        let (site, user, pass) = (site.clone(), user.clone(), pass.clone());
        Rc::new(move || {
            if SAVING.with(Cell::get) {
                return;
            }
            let s = site.borrow().text.trim().to_string();
            let u = user.borrow().text.trim().to_string();
            if s.is_empty() || u.is_empty() || pass.borrow().text.is_empty() {
                return;
            }
            // overwriting an entry takes a second Enter
            let exists = std::path::Path::new(&format!("{}/{s}.gpg", super::pass::store())).exists();
            if exists && CONFIRM.with(|c| c.borrow().as_deref() != Some(&s)) {
                notify_send("Pass", &format!("{s} exists"), "Press Enter again to overwrite it", vec![]);
                CONFIRM.with(|c| *c.borrow_mut() = Some(s));
                return;
            }
            CONFIRM.with(|c| c.borrow_mut().take());
            SAVING.with(|b| b.set(true));
            // wiped on drop
            let p = pass.borrow_mut().take_secret();
            runtime::spawn(async move {
                // sized up front: no reallocation leaves a copy behind
                let mut entry = String::with_capacity(p.len() + u.len() + 9);
                for part in [p.as_str(), "\nlogin: ", u.as_str(), "\n"] {
                    entry.push_str(part);
                }
                let out = output(&["pass", "insert", "-m", "-f", "--", &s], Some(entry.as_bytes())).await;
                wipe(&mut entry);
                SAVING.with(|b| b.set(false));
                match out {
                    Some(o) if o.code == 0 => {
                        window::set_visible("passSave", false);
                        copy_login(&u, p.as_str());
                        drop(p);
                        sync().await;
                    }
                    o => {
                        let err = o.map(|o| o.stderr.trim().to_string()).unwrap_or_else(|| "pass could not be started".into());
                        notify_send("Pass", "Unable to save password", &err, vec![]);
                    }
                }
            });
        })
    };

    let build = {
        let (site, user, pass) = (site.clone(), user.clone(), pass.clone());
        move |_: Option<&str>| {
            let base = window();
            let generated = generated().peek();
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
    // input_control clears the entries on show and hide
    let reset = Rc::new(|| {
        generated().set(false);
        CONFIRM.with(|c| c.borrow_mut().take());
    });
    let tab = {
        let generate = generate;
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
            EntryBinding { id: "site", entry: site, on_activate: Some(goto("user")), on_key: None },
            EntryBinding { id: "user", entry: user, on_activate: Some(goto("pass")), on_key: None },
            EntryBinding { id: "pass", entry: pass, on_activate: Some(save), on_key: Some(tab) },
        ],
        ..Default::default()
    });
    w.watch(&generated());
}

#[cfg(test)]
mod tests {
    #[test]
    fn password_has_every_class() {
        let p = super::random_password(20).unwrap();
        assert_eq!(p.len(), 20);
        for set in [super::LOWER, super::UPPER, super::DIGITS, super::SYMBOLS] {
            assert!(p.chars().any(|c| set.contains(c)));
        }
        assert_eq!(super::random_password(2).unwrap().len(), 4);
    }
}
