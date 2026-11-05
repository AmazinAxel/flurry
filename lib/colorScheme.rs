// The portal's color scheme, mirrored into the night light temperature (wl-gammarelay), and the
// theme toggle that sets it.

use crate::lib::dbus::{self, SignalFilter, method, set_prop};
use crate::lib::xdg;
use crate::settings;
use crate::ui::state::global_state;

global_state! {
    pub fn dark() -> bool = true;
}

/// Mirrors dark/light mode into the night light temperature.
fn apply_color_scheme(dark_mode: bool) {
    dark().set(dark_mode);
    if let Some((d, l)) = settings::NIGHT_LIGHT
        && let Some(bus) = dbus::session()
    {
        let k = (if dark_mode { d } else { l }).min(u16::MAX as u32) as u16;
        let mut m = set_prop("rs.wl-gammarelay", "/", "rs.wl.gammarelay", "Temperature", k);
        m.set_auto_start(false);
        bus.send(m);
    }
}

fn read_scheme() {
    let Some(bus) = dbus::session() else { return };
    let m = method("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings", "ReadOne")
        .append2("org.freedesktop.appearance", "color-scheme");
    bus.call_cb(m, |r| {
        // 1 = prefer dark; no answer (no portal yet) changes nothing: SettingChanged follows
        if let Some(v) = r.ok().and_then(|m| m.read1::<dbus::Variant<Box<dyn dbus::RefArg>>>().ok()).and_then(|v| v.0.as_u64()) {
            apply_color_scheme(v == 1);
        }
    });
}

thread_local! {
    /// flurry owns the dim: no read per press, reapplied when wl-gammarelay (re)appears
    static DIMMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn apply_dim() {
    let Some(bus) = dbus::session() else { return };
    let mut m = set_prop("rs.wl-gammarelay", "/", "rs.wl.gammarelay", "Brightness", if DIMMED.get() { 0.4 } else { 1.0 });
    m.set_auto_start(false);
    bus.send(m);
}

/// Toggles a 40% dim (wl-gammarelay's Brightness) on and off.
pub fn toggle_dim() {
    DIMMED.set(!DIMMED.get());
    apply_dim();
}

/// GVariant `a{smv}` of string values: dconf's change set for `ca.desrt.dconf.Writer.Change`.
fn dconf_changeset(kv: &[(&str, &str)]) -> Vec<u8> {
    // framing offsets, each in the smallest size that addresses the whole container
    fn frame(body: &mut Vec<u8>, ends: &[usize]) {
        let size = [1usize, 2, 4, 8].into_iter().find(|&s| s == 8 || body.len() + ends.len() * s < (1usize << (8 * s))).unwrap_or(8);
        for &e in ends {
            body.extend_from_slice(&e.to_le_bytes()[..size]);
        }
    }
    let pad = |b: &mut Vec<u8>| b.resize(b.len().next_multiple_of(8), 0);
    let mut out = vec![];
    let mut ends = vec![];
    for (k, v) in kv {
        let mut e = [k.as_bytes(), b"\0"].concat();
        let key_end = e.len();
        pad(&mut e);
        // Just(v): the variant (value, 0, its type) then the maybe's 0
        e.extend_from_slice(v.as_bytes());
        e.extend_from_slice(b"\0\0s\0");
        frame(&mut e, &[key_end]);
        pad(&mut out);
        out.extend_from_slice(&e);
        ends.push(out.len());
    }
    frame(&mut out, &ends);
    out
}

/// `nop flurry theme`: flips dark/light: the GNOME color scheme and GTK theme (dconf), GTK 4's
/// gtk.css symlink, and SIGUSR1 (dark) / SIGUSR2 (light) to the THEME units (foot's convention).
pub fn toggle_theme() {
    let Some((dark_theme, light_theme, dir, units)) = settings::THEME else { return };
    let dark_mode = !dark().peek();
    apply_color_scheme(dark_mode);
    let theme = if dark_mode { dark_theme } else { light_theme };
    if let Some(bus) = dbus::session() {
        const IFACE: &str = "/org/gnome/desktop/interface/";
        let set = dconf_changeset(&[
            (&format!("{IFACE}color-scheme"), if dark_mode { "prefer-dark" } else { "prefer-light" }),
            (&format!("{IFACE}gtk-theme"), theme),
        ]);
        bus.send(method("ca.desrt.dconf", "/ca/desrt/dconf/Writer/user", "ca.desrt.dconf.Writer", "Change").append1(set));
        let sig = if dark_mode { libc::SIGUSR1 } else { libc::SIGUSR2 };
        for u in units {
            bus.send(method("org.freedesktop.systemd1", "/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager", "KillUnit").append3(*u, "main", sig));
        }
    }
    // GTK 4 takes its theme from gtk.css only; swapped in atomically
    let css = format!("{}/gtk-4.0/gtk.css", xdg::config_dir());
    let tmp = format!("{css}.flurry");
    let _ = std::fs::remove_file(&tmp);
    if let Err(e) = std::os::unix::fs::symlink(format!("{dir}/{theme}/gtk-4.0/gtk.css"), &tmp).and_then(|_| std::fs::rename(&tmp, &css)) {
        eprintln!("flurry: {css}: {e}");
    }
}

pub fn start() {
    let Some(bus) = dbus::session() else { return };
    bus.subscribe(
        SignalFilter {
            sender: Some("org.freedesktop.portal.Desktop".into()),
            interface: Some("org.freedesktop.portal.Settings".into()),
            member: Some("SettingChanged".into()),
            ..Default::default()
        },
        |m| {
            if let Ok((ns, key, v)) = m.read3::<&str, &str, dbus::Variant<Box<dyn dbus::RefArg>>>()
                && ns == "org.freedesktop.appearance"
                && key == "color-scheme"
            {
                apply_color_scheme(v.0.as_u64() == Some(1));
            }
        },
    );
    read_scheme();
    bus.watch_name(
        "rs.wl-gammarelay",
        || {
            apply_color_scheme(dark().peek());
            apply_dim();
        },
        || {},
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn dconf_changeset_layout() {
        // a{smv} {"/a": Just(<"b">)}: "/a\0", pad to 8, "b\0\0s\0", key end 3; array end 14
        assert_eq!(super::dconf_changeset(&[("/a", "b")]), b"/a\0\0\0\0\0\0b\0\0s\0\x03\x0e");
        if let Ok(k) = std::env::var("FLURRY_DCONF_TEST") {
            println!("{:?}", super::dconf_changeset(&[(&k, "x"), (&format!("{k}2"), "yy")]));
        }
    }
}
