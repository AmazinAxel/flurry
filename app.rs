#![allow(non_snake_case, special_module_name)]
#![allow(clippy::type_complexity, clippy::module_inception)]

mod config;
mod lib;
mod settings;
mod ui;
mod widgets;

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;

use lib::{menuState, mediaPlayer, streamingMode::streaming_mode, xdg};
use ui::{runtime, window};
use widgets::{clipboard, focus, lockscreen::lockscreen, notifications::notifications as notifs, record::service, sideview, status};

/// Requests from sway keybinds (`flurry <cmd> [arg]`).
fn request(cmd: &str, arg: Option<&str>) {
    // locked: nothing may open behind (or over) the lock screen
    if (lockscreen::locked().peek() || ui::wayland::is_locking()) && !matches!(cmd, "lock" | "media") {
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

/// Binds the request socket; exits if another shell is already serving it.
fn bind_socket() -> Option<UnixDatagram> {
    let path = xdg::socket_path()?;
    if std::path::Path::new(&path).exists() {
        let probe = UnixDatagram::unbound().ok()?;
        let _ = probe.set_nonblocking(true);
        match probe.send_to(b"", &path) {
            Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => {
                let _ = std::fs::remove_file(&path); // stale
            }
            _ => std::process::exit(0),
        }
    }
    let sock = match UnixDatagram::bind(&path) {
        Ok(s) => s,
        // another shell won the race to bind it
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => std::process::exit(0),
        Err(_) => return None,
    };
    let _ = sock.set_nonblocking(true);
    Some(sock)
}

fn serve(sock: UnixDatagram) {
    let fd = sock.as_raw_fd();
    runtime::watch(fd, move || {
        let mut buf = [0u8; 512];
        while let Ok(n) = sock.recv(&mut buf) {
            let line = String::from_utf8_lossy(&buf[..n]);
            // `sideview plan` style: the first two words
            let mut words = line.trim().split(' ');
            if let Some(cmd) = words.next() {
                request(cmd, words.next());
            }
        }
    });
}

thread_local! {
    /// the sync reminder last sent: not sent again while it's still up
    static SYNC_NOTE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// Checked at startup, then once a day (a one-shot timer chain, no polling).
fn reminders() {
    let _ = runtime::timeout(86_400_000, reminders);
    if sync_reminder() {
        return;
    }
    let downloads = xdg::user_dir("DOWNLOAD");
    // no Downloads dir configured: user-dirs points it at $HOME, whose size means nothing here
    if downloads.trim_end_matches('/') == xdg::home_dir().trim_end_matches('/') {
        return;
    }
    // the walk can take a while on a full folder: off the loop
    ui::runtime::spawn(async move {
        let d = downloads.clone();
        // Greater than 100MB
        if ui::runtime::blocking(move || dir_size(std::path::Path::new(&d))).await.unwrap_or(0) > 100_000_000 {
            let argv: Vec<String> = settings::FILE_MANAGER.iter().map(|s| s.to_string()).chain([downloads]).collect();
            lib::notifySend::notify_send("Cleanup", "Empty Downloads", "", vec![(1, "View folder".into(), argv)]);
        }
    });
}

/// Reminds to sync when the last one is over a week old (wall clock, so time asleep counts);
/// true when due.
fn sync_reminder() -> bool {
    // lastSync: unix ms
    let last_sync = format!("{}/flurry/lastSync", xdg::state_dir());
    let last: u64 = std::fs::read_to_string(&last_sync).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let now = lib::time::now_secs() * 1000;
    if settings::SYNC_COMMAND.is_empty() || now.saturating_sub(last) <= 7 * 86_400_000 {
        return false;
    }
    if SYNC_NOTE.with(|n| n.get()).and_then(lib::notifd::get).is_some_and(|n| n.app_name == "Sync") {
        return true;
    }
    let argv: Vec<String> = settings::SYNC_COMMAND.iter().map(|s| s.to_string()).collect();
    let id = lib::notifd::send_local("Sync", "Sync system", "", &[("1".into(), "Update & Sync".into())], move |action| {
        if action == "1" {
            if let Some(dir) = std::path::Path::new(&last_sync).parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&last_sync, (lib::time::now_secs() * 1000).to_string());
            lib::apps::launch(&argv);
        }
    });
    SYNC_NOTE.with(|n| n.set(Some(id)));
    true
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
    let sock = bind_socket();
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
    if let Some(s) = sock {
        serve(s);
    }

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
        status::quicksettings::sound::start();
        status::bluetooth::start();
        status::network::start();
        status::modules::statusIcons::start();
        status::modules::mpris::start();
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
