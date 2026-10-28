use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::{background_section, header_style};
use crate::lib::clipboard::{copy, copy_login, pipe};
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::process::exec_str;
use crate::lib::searchDialog::{hex_sel, sel_fg};
use crate::lib::state::State;
use crate::lib::style::*;
use crate::ui::entry::{self, EntryRef, Look};
use crate::ui::node::{Align, Background, Node, Sides, Style, rgba};
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

/// Graphite `entry` (focus ring #dbdee5 inside) with flurry's square corners.
fn entry_node(base: &Style, e: &EntryRef, id: &'static str, focused: bool) -> Node {
    let mut s = base.inherit();
    s.min_height = 36.0;
    s.padding = Sides::vh(0.0, 8.0);
    s.background = Background::Color(rgba(255, 255, 255, 0.04));
    // Graphite entry text; :focus-within never matches (GTK does not consider layer windows active)
    s.color = rgba(255, 255, 255, 0.7);
    let look = Look { color: s.color, placeholder: s.color, selection_bg: hex_sel(), selection_fg: sel_fg(), caret: false };
    let _ = focused;
    Node::hbox(s.clone(), 0.0, vec![entry::text_node(e, s.inherit(), look)]).focusable(id).expand(true, false)
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
            copy(&p);
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
        move |v: &window::View| {
            let base = window();
            let generated = GENERATED.with(|g| g.peek());
            let mut icon = base.inherit();
            icon.color = if generated { blue2() } else { dark4() };
            icon.margin = Sides::trbl(0.0, 3.0, 0.0, 0.0);
            let content = Node::vbox(base.inherit(), 4.0, vec![
                entry_node(&base, &site, "site", v.focused("site")),
                entry_node(&base, &user, "user", v.focused("user")),
                Node::hbox(base.inherit(), 4.0, vec![
                    entry_node(&base, &pass, "pass", v.focused("pass")),
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
