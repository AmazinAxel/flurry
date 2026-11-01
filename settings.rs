// Replaced at build time by package.nix from programs.flurry.settings.
/// Sideview pages: name -> url ("assistant" -> "flurry:claude" is built in).
pub const SIDEVIEW: &[(&str, &str)] = &[];
/// Bluetooth device display names: advertised name -> shown name.
pub const BLUETOOTH_NAMES: &[(&str, &str)] = &[];
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
/// Notifications whose body starts with one of these are dropped.
pub const NOTIFICATION_FILTERS: &[&str] = &[];
/// Font fallback chains sorted by fontconfig at build time: (family, fontconfig weight, file, face
/// index, embolden). Empty here: dev builds read the dev shell's FLURRY_FONTS instead.
pub const FONTS: &[(&str, i32, &str, u32, bool)] = &[];
