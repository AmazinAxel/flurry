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
    // one datagram, no reply, never blocking: a busy shell (full queue) drops it, never stalls the keybind
    let sent = xdg::socket_path().zip(UnixDatagram::unbound().ok()).is_some_and(|(p, s)| {
        let _ = s.set_nonblocking(true);
        match s.send_to(args.join(" ").as_bytes(), p) {
            Ok(_) => true,
            Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
        }
    });
    if !sent {
        eprintln!("flurry is not running");
        std::process::exit(1);
    }
}
