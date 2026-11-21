use std::path::PathBuf;

/// Text renders nothing without a font table: package.nix writes it into settings.rs, the dev shell
/// passes it as FLURRY_FONTS. Said here once instead of at runtime.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=settings.rs");
    println!("cargo:rerun-if-env-changed=FLURRY_FONTS");
    let settings = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("settings.rs");
    let empty_table = std::fs::read_to_string(settings).is_ok_and(|s| s.lines().any(|l| l.starts_with("pub const FONTS:") && l.trim_end().ends_with("= &[];")));
    if empty_table && std::env::var("FLURRY_FONTS").unwrap_or_default().trim().is_empty() {
        println!("cargo:warning=no fonts: settings.rs FONTS is empty and FLURRY_FONTS is unset (use nix-shell / nix develop); text will not render");
    }
}
