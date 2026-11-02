use crate::settings;

/// Built-in sideview pages; settings.sideview overrides and extends them.
const DEFAULTS: &[(&str, &str)] = &[("assistant", "flurry:claude")];

pub fn sideview_url(page: &str) -> Option<&'static str> {
    settings::SIDEVIEW.iter().chain(DEFAULTS).find(|(p, _)| *p == page).map(|(_, u)| *u)
}
