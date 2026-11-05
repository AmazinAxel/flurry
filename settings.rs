// Replaced at build time by package.nix from programs.flurry.settings; the values here mirror
// package.nix's defaults for dev builds.
/// Sideview pages: name -> url ("assistant" -> "flurry:claude" is built in).
pub const SIDEVIEW: &[(&str, &str)] = &[];
/// Bluetooth device display names: advertised name -> shown name.
pub const BLUETOOTH_NAMES: &[(&str, &str)] = &[];
/// Sound output display names: advertised name (the sink's description) -> shown name.
pub const SOUND_NAMES: &[(&str, &str)] = &[];
/// mpd playlists (folders in the music dir) in cycle order, with a style color name
/// (red, yellow, purple, blue1..blue4; anything else is blue2).
pub const PLAYLISTS: &[(&str, &str)] = &[];
/// mpd crossfade seconds set on first play; 0 leaves mpd's setting alone.
pub const MPD_CROSSFADE: u32 = 0;
/// argv run by the weekly "Sync system" reminder's action; empty disables the reminder.
pub const SYNC_COMMAND: &[&str] = &[];
/// argv prefixes; the path is appended.
pub const FILE_MANAGER: &[&str] = &["xdg-open"];
pub const IMAGE_VIEWER: &[&str] = &["xdg-open"];
/// `nop flurry terminal`: TERMINAL, or TERMINAL_FLOATING (if set) when the focused workspace has a
/// window whose app_id starts with none of TERMINAL_TILED_WITH.
pub const TERMINAL: &[&str] = &["xdg-terminal-exec"];
pub const TERMINAL_FLOATING: &[&str] = &[];
pub const TERMINAL_TILED_WITH: &[&str] = &[];
/// Launcher app names (case-insensitive substrings) that start only after a focus break and run
/// for at most 5 minutes.
pub const FOCUS_BLOCKED: &[&str] = &["discord"];
/// Wallpaper directory; relative paths are under $HOME.
pub const WALLPAPER_DIR: &str = "Wallpapers";
/// argv run after saving a pass entry when argv[0] is on PATH; empty runs nothing.
pub const PASS_SYNC_COMMAND: &[&str] = &["pass-sync"];
/// wl-gammarelay temperatures (dark scheme, light scheme) in kelvin; None leaves it alone.
pub const NIGHT_LIGHT: Option<(u32, u32)> = Some((3500, 6500));
/// `nop flurry theme`: (dark GTK theme, light GTK theme, themes dir, units signalled); None: off.
pub const THEME: Option<(&str, &str, &str, &[&str])> = None;
/// Notifications whose body starts with one of these are dropped.
pub const NOTIFICATION_FILTERS: &[&str] = &[];
/// Font fallback chains sorted by fontconfig at build time: (family, fontconfig weight, file, face
/// index, embolden). Empty here: dev builds read the dev shell's FLURRY_FONTS instead.
pub const FONTS: &[(&str, i32, &str, u32, bool)] = &[];
