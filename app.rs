#![allow(non_snake_case, special_module_name)]
#![allow(clippy::type_complexity, clippy::module_inception)]

mod lib;
mod settings;
mod ui;
mod widgets;

use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use lib::{menuState, mediaPlayer, streamingMode::streaming_mode, xdg};
use ui::{runtime, window};
use widgets::{clipboard, focus, lockscreen::lockscreen, notifications::notifications as notifs, record::service, sideview, status};

/// Requests from sway keybinds (`nop flurry <cmd> [arg]`).
fn request(cmd: &str, arg: Option<&str>) {
    // locked: nothing may open behind (or over) the lock screen
    if (lockscreen::locked().peek() || ui::wayland::is_locking()) && !matches!(cmd, "lock" | "media" | "mpris" | "brightness" | "volume" | "dim") {
        return;
    }
    match cmd {
        "toggle" => {
            if let Some(w) = arg {
                window::toggle(w);
            }
        }
        "hideNotif" => notifs::clear_oldest_notification(),
        "randomWallpaper" => lib::wallpaper::random_wallpaper(),
        "lock" => lockscreen::lock_screen(),
        "invokeOldestNotif" => notifs::invoke_oldest_notification(arg.and_then(|a| a.parse().ok()).unwrap_or(0)),
        "toggleSideviewSize" => sideview::toggle_sideview_size(),
        "sideview" => sideview::show_page(arg.unwrap_or("")),
        "closeSideview" => sideview::close_sideview(),
        "hideSideview" => sideview::hide_sideview(),
        "toggleSideview" => sideview::toggle_sideview(),
        "toggleSideviewFocus" => sideview::toggle_sideview_focus(),
        "record" => {
            if service::is_rec().peek() {
                service::stop_rec();
            } else {
                window::toggle("recordMenu");
            }
        }
        "media" => match arg.unwrap_or("") {
            "next" => mediaPlayer::upd_track(true),
            "prev" => mediaPlayer::upd_track(false),
            "toggle" => mediaPlayer::play_pause(),
            "nextPlaylist" => mediaPlayer::chng_playlist(true),
            "prevPlaylist" => mediaPlayer::chng_playlist(false),
            _ => {}
        },
        "brightness" => lib::brightness::step(arg == Some("up")),
        "volume" => {
            let delta = match arg {
                Some("up") => Some(0.05),
                Some("down") => Some(-0.05),
                Some("mute") => None,
                _ => return,
            };
            if !lib::audio::step_volume(delta) {
                widgets::osd::osd::show_volume();
            }
        }
        "dim" => lib::colorScheme::toggle_dim(),
        "theme" => lib::colorScheme::toggle_theme(),
        "screenshot" => widgets::screenshot::screenshot(),
        "terminal" => lib::sway::open_terminal(),
        // + | - | move+ | move-
        "workspace" => {
            let a = arg.unwrap_or("");
            lib::sway::step_workspace(widgets::workspaces::focused().peek(), a.ends_with('+'), a.starts_with("move"));
        }
        "mpris" => match arg {
            Some("toggle") => status::modules::mpris::control("PlayPause"),
            Some("next") => status::modules::mpris::control("Next"),
            Some("prev") => status::modules::mpris::control("Previous"),
            _ => {}
        },
        "toggleStatus" => menuState::toggle_menu("status"),
        "toggleCalendar" => menuState::toggle_menu("calendar"),
        "toggleBluetooth" => menuState::toggle_menu("bluetooth"),
        "toggleWifi" => menuState::toggle_menu("wifi"),
        "closeMenu" => menuState::close_menu(),
        "toggleStreamingMode" => streaming_mode().set(!streaming_mode().peek()),
        "toggleFocus" => focus::toggle_focus(),
        "cancelBreak" => focus::cancel_break(),
        _ => {}
    }
}

/// Exits if another shell already holds the lock (held for life).
fn single_instance() {
    let Some(dir) = xdg::runtime_dir() else {
        eprintln!("flurry: no XDG_RUNTIME_DIR: no single-instance guard");
        return;
    };
    let lock = std::fs::OpenOptions::new().write(true).create(true).mode(0o600).open(format!("{dir}/flurry.lock"));
    match lock {
        Ok(f) if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 => std::mem::forget(f),
        Ok(_) => std::process::exit(0),
        Err(e) => eprintln!("flurry: lock file: {e}"),
    }
}

/// Sway keybinds `nop flurry <cmd> [arg]` arrive as binding events: no process per keypress.
fn serve_bindings() {
    lib::sway::subscribe(&["binding"], |e| {
        if let Some(line) = e.get("binding").get("command").str().and_then(parse_request) {
            request(line.0, line.1);
        }
    });
}

/// `nop flurry sideview plan b`: the command, then everything after it as one argument.
fn parse_request(line: &str) -> Option<(&str, Option<&str>)> {
    let rest = line.trim().strip_prefix("nop")?.trim_start().strip_prefix("flurry ")?.trim();
    Some(match rest.split_once(' ') {
        Some((cmd, arg)) => (cmd, Some(arg.trim())),
        None => (rest, None),
    })
}

thread_local! {
    /// each reminder last sent (id, unix secs): not again while it's up or for a day after
    static SYNC_NOTE: std::cell::Cell<Option<(u32, u64)>> = const { std::cell::Cell::new(None) };
    static DOWNLOADS_NOTE: std::cell::Cell<Option<(u32, u64)>> = const { std::cell::Cell::new(None) };
}

/// True when `note`'s reminder is still up or under a day old.
fn recent(note: &'static std::thread::LocalKey<std::cell::Cell<Option<(u32, u64)>>>, app: &str) -> bool {
    note.with(|n| n.get()).is_some_and(|(id, at)| lib::time::now_secs() < at + 86_400 || lib::notifd::get(id).is_some_and(|n| n.app_name == app))
}

/// Checked at startup, then once a day (a one-shot timer chain, no polling).
fn reminders() {
    let _ = runtime::timeout(86_400_000, reminders);
    sync_reminder();
    let downloads = xdg::user_dir("DOWNLOAD");
    // no Downloads dir configured: user-dirs points it at $HOME, whose size means nothing here
    if downloads.trim_end_matches('/') == xdg::home_dir().trim_end_matches('/') || recent(&DOWNLOADS_NOTE, "Cleanup") {
        return;
    }
    // the walk can take a while on a full folder: off the loop
    ui::runtime::spawn(async move {
        let d = downloads.clone();
        // Greater than 100MB
        if ui::runtime::blocking(move || dir_size(std::path::Path::new(&d))).await.unwrap_or(0) > 100_000_000 {
            let argv: Vec<String> = settings::FILE_MANAGER.iter().map(|s| s.to_string()).chain([downloads]).collect();
            let id = lib::notifySend::notify_send("Cleanup", "Empty Downloads", "", vec![(1, "View folder".into(), argv)]);
            DOWNLOADS_NOTE.with(|n| n.set(Some((id, lib::time::now_secs()))));
        }
    });
}

/// Reminds to sync when the last successful one is over a week old (wall clock, so time asleep
/// counts).
fn sync_reminder() {
    // lastSync: unix ms
    let last_sync = format!("{}/flurry/lastSync", xdg::state_dir());
    let last: u64 = std::fs::read_to_string(&last_sync).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let now = lib::time::now_secs() * 1000;
    if settings::SYNC_COMMAND.is_empty() || now.saturating_sub(last) <= 7 * 86_400_000 || recent(&SYNC_NOTE, "Sync") {
        return;
    }
    let id = lib::notifd::send_local("Sync", "Sync system", "", &[("1".into(), "Update & Sync".into())], move |action| {
        if action != "1" {
            return;
        }
        let last_sync = last_sync.clone();
        runtime::spawn(async move {
            if lib::process::status(settings::SYNC_COMMAND).await != Some(0) {
                return;
            }
            if let Some(dir) = std::path::Path::new(&last_sync).parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = xdg::write_atomic(&last_sync, (lib::time::now_secs() * 1000).to_string().as_bytes(), 0o644) {
                eprintln!("flurry: {last_sync}: {e}");
            }
        });
    });
    SYNC_NOTE.with(|n| n.set(Some((id, now / 1000))));
}

/// `du -sb`: apparent size of everything under `p`.
fn dir_size(p: &std::path::Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return 0 };
    let mut total = meta.len();
    if meta.is_dir()
        && let Ok(rd) = std::fs::read_dir(p)
    {
        for e in rd.flatten() {
            total += dir_size(&e.path());
        }
    }
    total
}

fn main() {
    // big temporaries (paint layers, decodes) straight from/back to the kernel: glibc's adaptive
    // threshold otherwise moves them onto the heap after the first free, where they stay resident
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 1 << 20); // blur buffers are ~3MB per card header
    }
    single_instance();
    if !ui::wayland::connect() {
        eprintln!("flurry: cannot connect to the Wayland display");
        std::process::exit(1);
    }

    // first frame: wallpaper + focus menu
    lib::time::start();
    lib::wallpaper::random_wallpaper();
    lib::wallpaper::wallpaper_window();
    focus::focus_menu();
    sideview::sideview();
    clipboard::clipboard::clipboard();
    clipboard::compress::compress_window();
    clipboard::compress::compress_progress();
    widgets::record::record::record_menu();
    focus::focus_task();
    widgets::osd::osd::osd();
    widgets::powermenu::powermenu::powermenu();
    lib::menuWindow::menu_window();
    widgets::workspaces::workspaces();
    notifs::notifications();
    widgets::launcher::launcher::launcher();
    widgets::pass::pass::pass();
    widgets::pass::passSave::pass_save();
    window::set_visible("focusMenu", true);
    serve_bindings();

    // services once the first frame has gone out: it never waits on them
    let start = || {
        lib::logind::start();
        lockscreen::start();
        if lockscreen::was_locked() {
            lockscreen::lock_screen(); // restarted while locked: lock again
        }
        lib::notifd::start();
        lib::battery::start();
        lib::bluetooth::start();
        lib::audio::start();
        lib::iwd::start();
        status::quicksettings::sound::start();
        status::bluetooth::start();
        status::network::start();
        status::modules::mpris::start();
        lib::sway::watch_terminal();
        mediaPlayer::watch();
        lib::brightness::monitor();
        service::start();
        lib::apps::reload();
        reminders();
        // monotonic timers stand still while suspended: a resume re-checks too
        lib::logind::sleeping().subscribe(|| {
            if !lib::logind::sleeping().peek() {
                sync_reminder();
            }
        });
        service::start_clipping_service();
        unsafe { libc::malloc_trim(0) };
    };
    match window::get("focusMenu") {
        Some(w) => w.after_commit(start),
        None => runtime::idle(start),
    }
    runtime::run();
}

#[cfg(test)]
mod tests {
    use super::parse_request;

    #[test]
    fn request_splits_at_first_space() {
        assert_eq!(parse_request("nop flurry launcher"), Some(("launcher", None)));
        assert_eq!(parse_request("nop  flurry sideview  plan b "), Some(("sideview", Some("plan b"))));
        assert_eq!(parse_request("nop other launcher"), None);
        assert_eq!(parse_request("exec flurry launcher"), None);
    }
}
