use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::inputControl::{Opts, input_control};
use crate::lib::logind;
use crate::ui::node::{Node, Style};
use crate::ui::window;
use crate::widgets::lockscreen::lockscreen::{lock_display_off, lock_screen, lock_then};

pub fn powermenu() {
    let hide: Rc<dyn Fn(&crate::ui::wayland::KeyEvent) -> bool> = Rc::new(|_| {
        window::set_visible("powermenu", false);
        true
    });
    input_control("powermenu", |_| Node::hbox(Style::default(), 0.0, vec![]), Opts {
        actions: vec![
            (Keysym::q, Rc::new(logind::poweroff)),
            (Keysym::s, Rc::new(|| lock_then(Box::new(logind::suspend)))),
            (Keysym::h, Rc::new(|| lock_then(Box::new(logind::hibernate)))),
            (Keysym::r, Rc::new(logind::reboot)),
            (Keysym::l, Rc::new(lock_screen)),
            (Keysym::d, Rc::new(lock_display_off)),
        ],
        on_key: Some(hide),
        ..Default::default()
    });
}
