// `flurry <cmd> [arg]`: the request client for sway keybinds. Kept apart from the shell so a keybind
// loads only libc, not the shell's dbus/pipewire/harfbuzz stack. `flurry` alone starts the shell.

#[allow(dead_code)]
#[path = "lib/xdg.rs"]
mod xdg;

use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        let shell = std::env::current_exe().map(|e| e.with_file_name("flurry-shell")).unwrap_or_else(|_| "flurry-shell".into());
        eprintln!("flurry: {}", std::process::Command::new(shell).exec());
        std::process::exit(1);
    }
    // one datagram, no reply: a busy shell never stalls the keybind
    let sent = xdg::socket_path().zip(UnixDatagram::unbound().ok()).is_some_and(|(p, s)| s.send_to(args.join(" ").as_bytes(), p).is_ok());
    if !sent {
        eprintln!("flurry is not running");
        std::process::exit(1);
    }
}
