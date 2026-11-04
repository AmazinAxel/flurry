// Desktop entries + fuzzy query.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::ui::runtime;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct App {
    pub name: String,
    pub executable: String,
    pub keywords: Vec<String>,
}

thread_local! {
    static APPS: RefCell<Vec<App>> = const { RefCell::new(Vec::new()) };
    /// the app dirs (resolved) and their mtimes at the last scan
    static STAMPS: RefCell<Vec<(PathBuf, Option<std::time::SystemTime>)>> = const { RefCell::new(Vec::new()) };
    /// inotify on every scanned dir: an edited, added or removed entry rescans
    static WATCH: RefCell<Option<OwnedFd>> = const { RefCell::new(None) };
}

fn data_dirs() -> Vec<PathBuf> {
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    std::iter::once(PathBuf::from(crate::lib::xdg::data_home())).chain(std::env::split_paths(&dirs)).collect()
}

pub fn app_dirs() -> Vec<PathBuf> {
    data_dirs().into_iter().map(|d| d.join("applications")).filter(|d| d.is_dir()).collect()
}

/// `ino` watches each dir before it is read, so a change during the scan still fires.
fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>, seen: &mut HashSet<PathBuf>, ino: RawFd) {
    // symlinked dirs are followed once each: a link loop ends here
    if !std::fs::canonicalize(dir).is_ok_and(|c| seen.insert(c)) {
        return;
    }
    if let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) {
        let mask = libc::IN_CREATE | libc::IN_DELETE | libc::IN_CLOSE_WRITE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_ATTRIB;
        unsafe { libc::inotify_add_watch(ino, c.as_ptr(), mask) };
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir() || (t.is_symlink() && p.is_dir()));
        if is_dir {
            walk(base, &p, out, seen, ino);
        } else if p.extension().is_some_and(|x| x == "desktop") && p.is_file() /* a FIFO would block the read */ {
            let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('/', "-");
            out.push((rel, p));
        }
    }
}

pub fn in_path(exe: &str) -> bool {
    if exe.contains('/') {
        return Path::new(exe).exists();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

/// Parses the [Desktop Entry] group; None when GIO would skip it (hidden, missing TryExec, not shown
/// here) or there is nothing to run.
fn parse(text: &str, desktops: &[String]) -> Option<App> {
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
    // terminal apps would start without a terminal
    if kv.get("Type").is_some_and(|t| *t != "Application") || yes("Hidden") || yes("NoDisplay") || yes("Terminal") {
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
    Some(App { name: kv.get("Name")?.to_string(), executable: kv.get("Exec").filter(|e| !e.is_empty())?.to_string(), keywords: list("Keywords") })
}

/// Rescans the desktop entries on a worker thread (hundreds of file reads).
pub fn reload() {
    runtime::spawn(async {
        // stamps first: a dir changing during the scan differs from them next time
        let Some((stamps, (apps, ino))) = runtime::blocking(|| (stamps(), scan())).await else { return };
        APPS.with(|a| *a.borrow_mut() = apps);
        STAMPS.with(|s| *s.borrow_mut() = stamps);
        watch(ino);
    });
}

/// Replaces the inotify watch; its first event rescans (which watches anew).
fn watch(ino: Option<OwnedFd>) {
    let old = WATCH.with(|w| w.borrow_mut().take());
    if let Some(f) = old {
        runtime::unwatch(f.as_raw_fd());
    }
    let Some(ino) = ino else { return };
    let fd = ino.as_raw_fd();
    WATCH.with(|w| *w.borrow_mut() = Some(ino));
    runtime::watch(fd, move || {
        runtime::unwatch(fd);
        reload();
    });
}

/// Rescans only if the set of app dirs or a top-level mtime changed (a switched profile swaps the
/// dirs themselves, which inotify on the old ones can't see), off the loop.
pub fn reload_if_changed() {
    runtime::spawn(async {
        let Some(now) = runtime::blocking(stamps).await else { return };
        if STAMPS.with(|s| *s.borrow() != now) {
            reload();
        }
    });
}

fn stamps() -> Vec<(PathBuf, Option<std::time::SystemTime>)> {
    app_dirs().into_iter().map(|d| (std::fs::canonicalize(&d).unwrap_or(d.clone()), std::fs::metadata(&d).and_then(|m| m.modified()).ok())).collect()
}

/// The apps, and an inotify fd watching every dir read (None if inotify is unavailable).
fn scan() -> (Vec<App>, Option<OwnedFd>) {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    let ino = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) });
    let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').map(String::from).collect();
    let mut seen = HashSet::new();
    let mut apps = vec![];
    for dir in app_dirs() {
        let mut files = vec![];
        walk(&dir, &dir, &mut files, &mut HashSet::new(), fd);
        for (id, path) in files {
            if !seen.insert(id) {
                continue; // earlier dirs shadow later ones
            }
            if let Some(a) = std::fs::read_to_string(&path).ok().and_then(|t| parse(&t, &desktops)) {
                apps.push(a);
            }
        }
    }
    (apps, ino)
}

/// GNOME Shell's fuzzy score; -10 when `pattern` doesn't match (or is empty).
fn fuzzy(pattern: &str, s: &str) -> i32 {
    let (pattern, s) = (pattern.as_bytes(), s.as_bytes());
    match best_match(pattern, s).filter(|_| s.len() >= pattern.len()) {
        Some(score) => 100 + score - (s.len() - pattern.len()) as i32,
        None => -10,
    }
}

/// Character boundary bonus: lower->Upper, or an alphanumeric after anything else.
fn bonus(cur: u8, prev: u8) -> i32 {
    let mut score = 0;
    if cur.is_ascii_uppercase() && prev.is_ascii_lowercase() {
        score += 30;
    }
    if cur.is_ascii_alphanumeric() && !prev.is_ascii_alphanumeric() {
        score += 30;
    }
    score
}

/// The best sum of per-character scores over every way of matching `pattern` in order (the
/// GNOME Shell recursive matcher, as an O(n·m) DP). After the first character, a match right
/// where the previous one left off gets +15, and its boundary bonus sees no previous character.
/// `Option`'s order (None below any Some) makes `max` the best of either.
fn best_match(pattern: &[u8], s: &[u8]) -> Option<i32> {
    let (&first, tail) = pattern.split_first()?;
    let n = s.len();
    let eq = |c: u8, p: u8| c.eq_ignore_ascii_case(&p);
    let prev = |j: usize| if j > 0 { s[j - 1] } else { 0 };
    // next[st]: best score of the rest of the pattern within s[st..]
    let mut next: Vec<Option<i32>> = vec![Some(0); n + 1];
    let mut cur = vec![None; n + 1];
    for &p in tail.iter().rev() {
        cur[n] = None;
        // best over matches at j > st (a jump: the real previous character counts)
        let mut later: Option<i32> = None;
        for st in (0..n).rev() {
            let rest = if eq(s[st], p) { next[st + 1] } else { None };
            cur[st] = rest.map(|v| v + 15 + bonus(s[st], 0)).max(later);
            later = later.max(rest.map(|v| v + bonus(s[st], prev(st))));
        }
        std::mem::swap(&mut next, &mut cur);
    }
    let mut best = None;
    for j in 0..n {
        if eq(s[j], first)
            && let Some(v) = next[j + 1]
        {
            let c = if j > 0 { bonus(s[j], s[j - 1]) } else { 15 };
            best = best.max(Some(v + c + (-5 * j as i32).max(-15)));
        }
    }
    best
}

/// Twice GNOME's name*2 + executable*0.5 + keywords*0.5 (integral), the program scored by its file
/// name: a /nix/store hash in its path would match anything.
fn score(term: &str, a: &App) -> i32 {
    let program = a.executable.split_whitespace().next().unwrap_or_default();
    let program = program.rsplit('/').next().unwrap_or_default();
    let kw = a.keywords.iter().map(|k| fuzzy(term, k)).fold(0, i32::max);
    fuzzy(term, &a.name) * 4 + fuzzy(term, program) + kw
}

/// The best `limit` matches, best first.
pub fn fuzzy_query(term: &str, limit: usize) -> Vec<App> {
    APPS.with(|a| {
        let apps = a.borrow();
        let mut hits: Vec<(i32, &App)> = apps.iter().map(|a| (score(term, a), a)).filter(|(s, _)| *s > 0).collect();
        hits.sort_by_key(|h| std::cmp::Reverse(h.0)); // stable: ties keep scan order
        hits.into_iter().take(limit).map(|(_, a)| a.clone()).collect()
    })
}

/// Desktop-entry Exec line -> argv: string escapes (`\s`, `\\`...), double-quoted args, `%%` -> `%`,
/// field codes (`%U`, `%f`...) dropped.
pub fn exec_argv(exec: &str) -> Vec<String> {
    let mut unescaped = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            unescaped.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => unescaped.push(' '),
            Some('n') => unescaped.push('\n'),
            Some('t') => unescaped.push('\t'),
            Some('r') => unescaped.push('\r'),
            Some('\\') => unescaped.push('\\'), // `\\"` in the file is the Exec-level `\"`
            Some(o) => {
                unescaped.push('\\');
                unescaped.push(o);
            }
            None => {}
        }
    }
    let mut argv = vec![];
    let mut cur = String::new();
    let (mut has, mut quoted) = (false, false);
    let mut chars = unescaped.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    has = true;
                }
            }
            '%' => match chars.next() {
                Some('%') => {
                    cur.push('%');
                    has = true;
                }
                Some(n) if n.is_ascii_alphabetic() => {}
                Some(n) => {
                    cur.push('%');
                    cur.push(n);
                    has = true;
                }
                None => {
                    cur.push('%');
                    has = true;
                }
            },
            c if c.is_whitespace() && !quoted => {
                if has {
                    argv.push(std::mem::take(&mut cur));
                }
                has = false;
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        argv.push(cur);
    }
    argv
}

/// `argv` as one `sh -c` line that sway's command parser passes through untouched: double quotes
/// keep `,` `;` and spaces inside one arg for both, and `\\` `"` `$` `` ` `` are backslash-escaped.
/// Sway skips variable replacement only for a `$` after a single backslash, so a `\\` right before
/// a `$` ends the quoted run (`\\""\$`).
pub fn shell_line(argv: &[String]) -> String {
    let mut line = String::new();
    for a in argv {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push('"');
        for c in a.chars() {
            if c == '$' && line.ends_with("\\\\") {
                line.push_str("\"\"");
            }
            if matches!(c, '\\' | '"' | '$' | '`') {
                line.push('\\');
            }
            line.push(c);
        }
        line.push('"');
    }
    line
}

/// Starts `argv` as sway's child, outside flurry's systemd cgroup (a shell restart never takes it down).
/// `exec exec`: sway strips the quotes of a lone quoted argument (so always 2+), and the app replaces
/// the shell. A newline would end the sway command: such an argv isn't run.
pub fn launch(argv: &[String]) {
    if !argv.is_empty() && !argv.iter().any(|a| a.contains(['\n', '\r'])) {
        crate::lib::sway::command(&format!("exec exec {}", shell_line(argv)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_prefix_and_separators() {
        assert!(fuzzy("fo", "Foot") > fuzzy("fo", "Info Tool"));
        assert_eq!(fuzzy("zz", "Foot"), -10);
        assert_eq!(fuzzy("", "x"), -10);
    }

    /// the original recursive matcher: the DP must score exactly like it
    fn recurse(pattern: &[u8], s: &[u8], score: i32, first: bool) -> Option<i32> {
        if pattern.is_empty() {
            return Some(score);
        }
        let mut best: Option<i32> = None;
        let mut offset = 0;
        while let Some(i) = s[offset..].iter().position(|c| c.eq_ignore_ascii_case(&pattern[0])) {
            offset += i;
            let jump = if first { (-5 * offset as i32).max(-15) } else { 0 };
            let c = match (first, offset) {
                (true, 0) => 15,
                (false, 0) => 15 + bonus(s[0], 0),
                _ => bonus(s[offset], s[offset - 1]),
            } + jump;
            let Some(sub) = recurse(&pattern[1..], &s[offset + 1..], c, false) else { break };
            best = best.max(Some(sub));
            offset += 1;
        }
        best.map(|b| score + b)
    }

    #[test]
    fn dp_matches_recursive_scores() {
        let words = ["Foot", "Info Tool", "fooBar foo", "GNOME Terminal", "aaaa", "a-b_c d", "xFxOxO", "Firefox Web Browser", "f"];
        let pats = ["f", "fo", "foo", "aa", "ter", "bc", "fx", "ob", "zz", "FOO", "web"];
        for w in words {
            for p in pats {
                let want = if w.len() < p.len() { None } else { recurse(p.as_bytes(), w.as_bytes(), 0, true) };
                assert_eq!(best_match(p.as_bytes(), w.as_bytes()).filter(|_| w.len() >= p.len()), want, "{p} in {w}");
            }
        }
    }

    #[test]
    fn desktop_entry_filters() {
        let d = vec!["sway".to_string()];
        assert!(parse("[Desktop Entry]\nName=A\nExec=a\nNoDisplay=true\n", &d).is_none());
        assert!(parse("[Desktop Entry]\nName=A\nExec=a\nOnlyShowIn=GNOME;\n", &d).is_none());
        assert!(parse("[Desktop Entry]\nName=A\n", &d).is_none());
        let a = parse("[Desktop Entry]\nName=A\nExec=a %U\nKeywords=x;y;\n[Desktop Action n]\nName=B\n", &d).unwrap();
        assert_eq!((a.name.as_str(), a.keywords.len()), ("A", 2));
    }

    #[test]
    fn exec_lines() {
        let v = |s: &str| exec_argv(s);
        assert_eq!(v("foot %U --x %f"), ["foot", "--x"]);
        assert_eq!(v(r#"sh -c "echo \\"a b\\"; echo 100%%" %u"#), ["sh", "-c", r#"echo "a b"; echo 100%"#]);
        assert_eq!(v(r#""a\sb" c\\\\d"#), ["a b", r"c\d"]);
        assert_eq!(v(r#"x """#), ["x", ""]);
        assert_eq!(shell_line(&["a b,c;d".into(), r#"$x"\`"#.into()]), r#""a b,c;d" "\$x\"\\\`""#);
        assert_eq!(shell_line(&[r"a\$b".into()]), r#""a\\""\$b""#);
    }

    #[test]
    fn single_letter_query_filters() {
        let app = |n: &str| App { name: n.into(), ..Default::default() };
        APPS.with(|a| *a.borrow_mut() = vec![app("Zed"), app("Foot"), app("Info")]);
        let names: Vec<String> = fuzzy_query("f", 5).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["Foot", "Info"]);
    }
}
