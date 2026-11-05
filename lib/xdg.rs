// GLib user dirs: XDG base dirs and user-dirs.dirs special dirs.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn base(var: &str, fallback: &str) -> String {
    std::env::var(var).ok().filter(|v| v.starts_with('/')).unwrap_or_else(|| format!("{}/{fallback}", home_dir()))
}

pub fn state_dir() -> String {
    base("XDG_STATE_HOME", ".local/state")
}
pub fn cache_dir() -> String {
    base("XDG_CACHE_HOME", ".cache")
}
pub fn config_dir() -> String {
    base("XDG_CONFIG_HOME", ".config")
}
pub fn data_home() -> String {
    base("XDG_DATA_HOME", ".local/share")
}
/// No fallback: a shared /tmp path could be squatted by another user.
pub fn runtime_dir() -> Option<String> {
    std::env::var("XDG_RUNTIME_DIR").ok().filter(|v| v.starts_with('/'))
}

/// tmp + fsync + rename: a crash never leaves a torn or empty file under the real name.
pub fn write_atomic(path: &str, data: &[u8], mode: u32) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp");
    let res = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut f| f.write_all(data).and_then(|_| f.sync_all()))
        .and_then(|_| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// g_get_user_special_dir: XDG_<NAME>_DIR from user-dirs.dirs, else $HOME/<Name>.
pub fn user_dir(name: &str) -> String {
    let key = format!("XDG_{name}_DIR=");
    let file = std::fs::read_to_string(format!("{}/user-dirs.dirs", config_dir())).unwrap_or_default();
    for line in file.lines() {
        if let Some(v) = line.trim().strip_prefix(&key) {
            return v.trim_matches('"').replace("$HOME", &home_dir());
        }
    }
    let mut n = name.to_lowercase();
    n[..1].make_ascii_uppercase();
    format!("{}/{n}", home_dir())
}
