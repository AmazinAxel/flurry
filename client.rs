// `flurry <cmd> [arg]`: the request client for sway keybinds. Kept apart from the shell so a keybind
// loads only libc, not the shell's dbus/pipewire/fontconfig/harfbuzz stack. `flurry` alone starts the shell.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        let shell = std::env::current_exe().map(|e| e.with_file_name("flurry-shell")).unwrap_or_else(|_| "flurry-shell".into());
        eprintln!("flurry: {}", std::process::Command::new(shell).exec());
        std::process::exit(1);
    }
    // same path as the shell's socket_path() (lib/xdg.rs runtime_dir)
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/tmp/flurry-{}", unsafe { libc::getuid() }));
    let Ok(mut s) = UnixStream::connect(format!("{dir}/flurry.sock")) else {
        eprintln!("flurry is not running");
        std::process::exit(1);
    };
    let _ = s.write_all((args.join(" ") + "\n").as_bytes());
    let _ = s.read(&mut [0u8; 16]);
}
