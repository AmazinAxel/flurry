// GLib user dirs: XDG base dirs and user-dirs.dirs special dirs.

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn base(var: &str, fallback: &str) -> String {
    std::env::var(var).ok().filter(|v| v.starts_with('/')).unwrap_or_else(|| format!("{}/{fallback}", home()))
}

pub fn home_dir() -> String {
    home()
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
pub fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/tmp/flurry-{}", unsafe { libc::getuid() }))
}

/// g_get_user_special_dir: XDG_<NAME>_DIR from user-dirs.dirs, else $HOME/<Name>.
pub fn user_dir(name: &str) -> String {
    let key = format!("XDG_{name}_DIR=");
    let file = std::fs::read_to_string(format!("{}/user-dirs.dirs", config_dir())).unwrap_or_default();
    for line in file.lines() {
        if let Some(v) = line.trim().strip_prefix(&key) {
            return v.trim_matches('"').replace("$HOME", &home());
        }
    }
    let mut n = name.to_lowercase();
    n[..1].make_ascii_uppercase();
    format!("{}/{n}", home())
}
