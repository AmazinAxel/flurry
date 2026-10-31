use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::{background_section, header_style};
use crate::lib::clipboard::{copy_login, copy_sensitive, pipe};
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::process::exec_str;
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

fn random_below(n: usize) -> usize {
    let mut x = 0u64;
    unsafe { libc::getrandom((&raw mut x).cast(), 8, 0) };
    (x % n as u64) as usize
}

fn pick(set: &str) -> char {
    set.as_bytes()[random_below(set.len())] as char
}

pub fn random_password(length: usize) -> String {
    let alphabet = format!("{LOWER}{UPPER}{DIGITS}{SYMBOLS}");
    let mut chars = vec![pick(LOWER), pick(UPPER), pick(DIGITS), pick(SYMBOLS)];
    while chars.len() < length {
        chars.push(pick(&alphabet));
    }
    for i in (1..chars.len()).rev() {
        let j = random_below(i + 1);
        chars.swap(i, j);
    }
    chars.into_iter().collect()
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
            let p = random_password(20);
            pass.borrow_mut().set_text(&p);
            GENERATED.with(|g| g.set(true));
            runtime::spawn(async move { copy_sensitive(&p).await });
        })
    };

    let save = {
        let (site, user, pass) = (site.clone(), user.clone(), pass.clone());
        Rc::new(move || {
            let s = site.borrow().text.trim().to_string();
            let u = user.borrow().text.trim().to_string();
            let p = pass.borrow().text.clone();
            if s.is_empty() || u.is_empty() || p.is_empty() {
                return;
            }
            window::set_visible("passSave", false);
            runtime::spawn(async move {
                if !pipe(&["pass", "insert", "-m", "-f", &s], &format!("{p}\nlogin: {u}\n")).await {
                    crate::lib::notifySend::notify_send("Pass", "Unable to save password", "", vec![]);
                    return;
                }
                copy_login(&u, &p).await;
                if exec_str("pass-sync").await.is_err() {
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
            EntryBinding { id: "site", entry: site, on_activate: Some(goto("user")), on_key: None },
            EntryBinding { id: "user", entry: user, on_activate: Some(goto("pass")), on_key: None },
            EntryBinding { id: "pass", entry: pass, on_activate: Some(save), on_key: Some(tab) },
        ],
        ..Default::default()
    });
    w.watch(&GENERATED.with(Clone::clone));
}
