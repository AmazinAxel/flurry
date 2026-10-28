use crate::settings;

/// Sideview pages: defaults, overridden/extended by settings.sideview.
pub fn sideview() -> Vec<(&'static str, &'static str)> {
    let mut pages = vec![("assistant", "https://claude.ai/new/"), ("custom", "https://beta-docs.skriptlang.org/syntaxes/")];
    for (k, v) in settings::SIDEVIEW {
        match pages.iter_mut().find(|(p, _)| p == k) {
            Some(p) => p.1 = v,
            None => pages.push((k, v)),
        }
    }
    pages
}

pub fn sideview_url(page: &str) -> Option<&'static str> {
    sideview().into_iter().find(|(p, _)| *p == page).map(|(_, u)| u)
}
