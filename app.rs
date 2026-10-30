#![allow(non_snake_case, special_module_name)]
#![allow(clippy::type_complexity, clippy::module_inception)]

mod config;
mod lib;
mod settings;
mod ui;
mod widgets;

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;

use lib::{menuState, mediaPlayer, streamingMode::streaming_mode, xdg};
use ui::{runtime, window};
use widgets::{clipboard, focus, lockscreen::lockscreen, notifications::notifications as notifs, record::service, sideview, status};

fn socket_path() -> String {
    format!("{}/flurry.sock", xdg::runtime_dir())
}

/// Requests from sway keybinds (`flurry <cmd> [arg]`).
fn request(cmd: &str, arg: Option<&str>) {
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

fn serve() {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else { return };
    let _ = listener.set_nonblocking(true);
    let fd = listener.as_raw_fd();
    runtime::watch(fd, move || {
        while let Ok((mut s, _)) = listener.accept() {
            let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(200)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            while let Ok(n) = s.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') {
                    break;
                }
            }
            let line = String::from_utf8_lossy(&buf).trim().to_string();
            // `sideview plan` style: the first two words
            let mut words = line.split(' ');
            if let Some(cmd) = words.next() {
                request(cmd, words.next());
            }
            let _ = s.write_all(b"\n");
        }
    });
}

fn reminders() {
    let last: u64 = std::fs::read_to_string(format!("{}/flurry/lastSync", xdg::state_dir())).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let has_sync = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("sys-sync").is_file()));
    if has_sync && now.saturating_sub(last) > 540_000_000 {
        // Last sync was ~7 days ago
        lib::notifySend::notify_send("Sync", "Sync system", "", vec![(
            1,
            "Update & Sync".into(),
            r#"footclient -e fish -c 'sys-sync; echo "Press a key to exit"; read --nchars=1'"#.into(),
        )]);
        return;
    }
    let downloads = xdg::user_dir("DOWNLOAD");
    // the walk can take a while on a full folder: off the loop
    ui::runtime::spawn(async move {
        let d = downloads.clone();
        // Greater than 100MB
        if ui::runtime::blocking(move || dir_size(std::path::Path::new(&d))).await.unwrap_or(0) > 100_000_000 {
            lib::notifySend::notify_send("Cleanup", "Empty Downloads", "", vec![(1, "View folder".into(), format!("nemo {downloads}"))]);
        }
    });
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
        libc::mallopt(libc::M_MMAP_THRESHOLD, 4 << 20);
    }
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
    serve();

    // services once the first frame has gone out: it never waits on them
    let start = || {
        lib::logind::start();
        lockscreen::start();
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
        service::start_clipping_service();
        ui::text::release_fontconfig();
        unsafe { libc::malloc_trim(0) };
    };
    match window::get("focusMenu") {
        Some(w) => w.after_commit(start),
        None => runtime::idle(start),
    }
    runtime::run();
}
