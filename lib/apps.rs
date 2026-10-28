// Desktop entries + AstalApps' fuzzy query (lib/apps/src/{apps,application,fuzzy}.vala).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct App {
    pub id: String,
    pub name: String,
    pub executable: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub categories: Vec<String>,
    pub frequency: i64,
}

thread_local! {
    static APPS: RefCell<Vec<App>> = const { RefCell::new(Vec::new()) };
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    home.into_iter().chain(std::env::split_paths(&dirs)).collect()
}

pub fn app_dirs() -> Vec<PathBuf> {
    data_dirs().into_iter().map(|d| d.join("applications")).filter(|d| d.is_dir()).collect()
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(base, &p, out);
        } else if p.extension().is_some_and(|x| x == "desktop") {
            let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('/', "-");
            out.push((rel, p));
        }
    }
}

fn in_path(exe: &str) -> bool {
    if exe.contains('/') {
        return Path::new(exe).exists();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

/// Parses the [Desktop Entry] group; None when GIO would skip it (hidden, missing TryExec, not shown here).
fn parse(id: &str, text: &str, desktops: &[String]) -> Option<App> {
    let mut in_group = false;
    let mut kv: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            kv.entry(k.trim()).or_insert(v.trim());
        }
    }
    let list = |k: &str| kv.get(k).map(|v| v.split(';').filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>()).unwrap_or_default();
    let yes = |k: &str| kv.get(k).is_some_and(|v| *v == "true");
    if kv.get("Type").is_some_and(|t| *t != "Application") || yes("Hidden") || yes("NoDisplay") {
        return None;
    }
    if let Some(t) = kv.get("TryExec")
        && !in_path(t)
    {
        return None;
    }
    let only = list("OnlyShowIn");
    if !only.is_empty() && !only.iter().any(|d| desktops.contains(d)) {
        return None;
    }
    if list("NotShowIn").iter().any(|d| desktops.contains(d)) {
        return None;
    }
    Some(App {
        id: id.into(),
        name: kv.get("Name")?.to_string(),
        executable: kv.get("Exec").unwrap_or(&"").to_string(),
        description: kv.get("Comment").unwrap_or(&"").to_string(),
        keywords: list("Keywords"),
        categories: list("Categories"),
        frequency: 0,
    })
}

fn frequents() -> HashMap<String, i64> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|c| c.join("astal/apps-frequents.json"));
    let Some(text) = cache.and_then(|c| std::fs::read_to_string(c).ok()) else { return HashMap::new() };
    match crate::lib::json::parse(&text) {
        Some(crate::lib::json::Json::Obj(m)) => m.into_iter().filter_map(|(k, v)| Some((k, v.int()?))).collect(),
        _ => HashMap::new(),
    }
}

pub fn reload() {
    let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').map(String::from).collect();
    let freq = frequents();
    let mut seen = HashSet::new();
    let mut apps = vec![];
    for dir in app_dirs() {
        let mut files = vec![];
        walk(&dir, &dir, &mut files);
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue; // earlier dirs shadow later ones
            }
            if let Some(mut a) = std::fs::read_to_string(&path).ok().and_then(|t| parse(&id, &t, &desktops)) {
                a.frequency = *freq.get(&id).unwrap_or(&0);
                apps.push(a);
            }
        }
    }
    APPS.with(|a| *a.borrow_mut() = apps);
}

/// Rescans when any applications dir changes.
pub fn watch() {
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return;
    }
    for d in app_dirs() {
        if let Ok(c) = std::ffi::CString::new(d.to_string_lossy().as_bytes()) {
            unsafe { libc::inotify_add_watch(fd, c.as_ptr(), libc::IN_CREATE | libc::IN_DELETE | libc::IN_MODIFY | libc::IN_MOVED_TO | libc::IN_MOVED_FROM) };
        }
    }
    let rescan = crate::ui::runtime::idle_once(reload);
    crate::ui::runtime::watch(fd, move || {
        let mut buf = [0u8; 4096];
        while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        rescan();
    });
}

fn fuzzy_match_string(pattern: &[u8], s: &[u8]) -> i32 {
    let not_found = -10;
    if pattern.is_empty() {
        return 100;
    }
    if s.len() < pattern.len() {
        return not_found;
    }
    match recurse(pattern, s, 100, true) {
        Some(score) => score - (s.len() - pattern.len()) as i32,
        None => not_found,
    }
}

fn recurse(pattern: &[u8], s: &[u8], score: i32, first: bool) -> Option<i32> {
    if pattern.is_empty() {
        return Some(score);
    }
    let search = pattern[0].to_ascii_lowercase();
    let mut best: Option<i32> = None;
    let mut offset = 0;
    while let Some(i) = s[offset..].iter().position(|c| c.to_ascii_lowercase() == search) {
        offset += i;
        let found = recurse(&pattern[1..], &s[offset + 1..], compute(offset, first, s, offset), false);
        let Some(sub) = found else { break };
        best = Some(best.map_or(sub, |b| b.max(sub)));
        offset += 1;
    }
    best.map(|b| score + b)
}

fn compute(jump: usize, first: bool, m: &[u8], idx: usize) -> i32 {
    let mut score = 0;
    if !first && jump == 0 {
        score += 15;
    }
    if !first || jump > 0 {
        let cur = m[idx];
        let prev = if idx > 0 { m[idx - 1] } else { 0 };
        if cur.is_ascii_uppercase() && prev.is_ascii_lowercase() {
            score += 30;
        }
        if cur.is_ascii_alphanumeric() && !prev.is_ascii_alphanumeric() {
            score += 30;
        }
    }
    if first && jump == 0 {
        score += 15;
    }
    if first {
        score += (-5 * jump as i32).max(-15);
    }
    score
}

fn fuzzy(term: &str, s: &str) -> i32 {
    fuzzy_match_string(term.as_bytes(), s.as_bytes())
}

/// name*2 + entry*0 + executable*0.5 + description*0 + keywords*0.5 + categories*0
fn score(term: &str, a: &App) -> f64 {
    let kw = a.keywords.iter().map(|k| fuzzy(term, k)).fold(0, i32::max);
    fuzzy(term, &a.name) as f64 * 2.0 + fuzzy(term, &a.executable) as f64 * 0.5 + kw as f64 * 0.5
}

pub fn fuzzy_query(term: &str) -> Vec<App> {
    let mut arr = APPS.with(|a| a.borrow().clone());
    if term.is_empty() {
        arr.sort_by(|a, b| b.frequency.cmp(&a.frequency));
        return arr;
    }
    if term.chars().count() == 1 {
        arr.retain(|a| score(term, a) != 0.0);
        arr.sort_by(|a, b| b.frequency.cmp(&a.frequency));
        return arr;
    }
    arr.retain(|a| score(term, a) >= 0.5);
    arr.sort_by(|a, b| {
        let (s1, s2) = (score(term, a), score(term, b));
        if s1 == s2 { b.frequency.cmp(&a.frequency) } else { s2.total_cmp(&s1) }
    });
    arr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_prefix_and_separators() {
        assert!(fuzzy("fo", "Foot") > fuzzy("fo", "Info Tool"));
        assert_eq!(fuzzy("zz", "Foot"), -10);
        assert_eq!(fuzzy("", "x"), 100);
    }

    #[test]
    fn desktop_entry_filters() {
        let d = vec!["sway".to_string()];
        assert!(parse("a.desktop", "[Desktop Entry]\nName=A\nExec=a\nNoDisplay=true\n", &d).is_none());
        assert!(parse("a.desktop", "[Desktop Entry]\nName=A\nExec=a\nOnlyShowIn=GNOME;\n", &d).is_none());
        let a = parse("a.desktop", "[Desktop Entry]\nName=A\nExec=a %U\nKeywords=x;y;\n[Desktop Action n]\nName=B\n", &d).unwrap();
        assert_eq!((a.name.as_str(), a.keywords.len()), ("A", 2));
    }
}
