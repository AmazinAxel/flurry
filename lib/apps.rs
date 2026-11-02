// Desktop entries + fuzzy query.

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
}

thread_local! {
    static APPS: RefCell<Vec<App>> = const { RefCell::new(Vec::new()) };
    /// the app dirs (resolved) and their mtimes at the last scan
    static STAMPS: RefCell<Vec<(PathBuf, Option<std::time::SystemTime>)>> = const { RefCell::new(Vec::new()) };
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

fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>, seen: &mut HashSet<PathBuf>) {
    // symlinked dirs are followed once each: a link loop ends here
    if !std::fs::canonicalize(dir).is_ok_and(|c| seen.insert(c)) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir() || (t.is_symlink() && p.is_dir()));
        if is_dir {
            walk(base, &p, out, seen);
        } else if p.extension().is_some_and(|x| x == "desktop") {
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
    Some(App {
        id: id.into(),
        name: kv.get("Name")?.to_string(),
        executable: kv.get("Exec").unwrap_or(&"").to_string(),
        description: kv.get("Comment").unwrap_or(&"").to_string(),
        keywords: list("Keywords"),
        categories: list("Categories"),
    })
}

/// Rescans the desktop entries on a worker thread (hundreds of file reads).
pub fn reload() {
    crate::ui::runtime::spawn(async {
        if let Some((apps, stamps)) = crate::ui::runtime::blocking(|| (scan(), stamps())).await {
            APPS.with(|a| *a.borrow_mut() = apps);
            STAMPS.with(|s| *s.borrow_mut() = stamps);
        }
    });
}

/// Rescans only if an app dir changed (installed/removed entries, a switched profile), off the loop.
pub fn reload_if_changed() {
    crate::ui::runtime::spawn(async {
        let Some(now) = crate::ui::runtime::blocking(stamps).await else { return };
        if STAMPS.with(|s| *s.borrow() != now) {
            reload();
        }
    });
}

fn stamps() -> Vec<(PathBuf, Option<std::time::SystemTime>)> {
    app_dirs().into_iter().map(|d| (std::fs::canonicalize(&d).unwrap_or(d.clone()), std::fs::metadata(&d).and_then(|m| m.modified()).ok())).collect()
}

fn scan() -> Vec<App> {
    let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').map(String::from).collect();
    let mut seen = HashSet::new();
    let mut apps = vec![];
    for dir in app_dirs() {
        let mut files = vec![];
        walk(&dir, &dir, &mut files, &mut HashSet::new());
        for (id, path) in files {
            if !seen.insert(id.clone()) {
                continue; // earlier dirs shadow later ones
            }
            if let Some(a) = std::fs::read_to_string(&path).ok().and_then(|t| parse(&id, &t, &desktops)) {
                apps.push(a);
            }
        }
    }
    apps
}

fn fuzzy_match_string(pattern: &[u8], s: &[u8]) -> i32 {
    let not_found = -10;
    if pattern.is_empty() {
        return 100;
    }
    if s.len() < pattern.len() {
        return not_found;
    }
    match best_match(pattern, s) {
        Some(score) => 100 + score - (s.len() - pattern.len()) as i32,
        None => not_found,
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

fn max_opt(a: Option<i32>, b: Option<i32>) -> Option<i32> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The best sum of per-character scores over every way of matching `pattern` in order (the
/// GNOME Shell recursive matcher, as an O(n·m) DP). After the first character, a match right
/// where the previous one left off gets +15, and its boundary bonus sees no previous character.
fn best_match(pattern: &[u8], s: &[u8]) -> Option<i32> {
    let n = s.len();
    let eq = |c: u8, p: u8| c.eq_ignore_ascii_case(&p);
    let prev = |j: usize| if j > 0 { s[j - 1] } else { 0 };
    // next[st]: best score of the rest of the pattern within s[st..]
    let mut next: Vec<Option<i32>> = vec![Some(0); n + 1];
    for &p in pattern[1..].iter().rev() {
        let mut cur = vec![None; n + 1];
        // best over matches at j > st (a jump: the real previous character counts)
        let mut later: Option<i32> = None;
        for st in (0..n).rev() {
            let rest = if eq(s[st], p) { next[st + 1] } else { None };
            cur[st] = max_opt(rest.map(|v| v + 15 + bonus(s[st], 0)), later);
            later = max_opt(later, rest.map(|v| v + bonus(s[st], prev(st))));
        }
        next = cur;
    }
    let mut best = None;
    for j in 0..n {
        if eq(s[j], pattern[0])
            && let Some(v) = next[j + 1]
        {
            let c = if j > 0 { bonus(s[j], s[j - 1]) } else { 15 };
            best = max_opt(best, Some(v + c + (-5 * j as i32).max(-15)));
        }
    }
    best
}

fn fuzzy(term: &str, s: &str) -> i32 {
    fuzzy_match_string(term.as_bytes(), s.as_bytes())
}

/// name*2 + entry*0 + executable*0.5 + description*0 + keywords*0.5 + categories*0
fn score(term: &str, a: &App) -> f64 {
    let kw = a.keywords.iter().map(|k| fuzzy(term, k)).fold(0, i32::max);
    fuzzy(term, &a.name) as f64 * 2.0 + fuzzy(term, &a.executable) as f64 * 0.5 + kw as f64 * 0.5
}

/// The best `limit` matches, best first.
pub fn fuzzy_query(term: &str, limit: usize) -> Vec<App> {
    APPS.with(|a| {
        let apps = a.borrow();
        if term.is_empty() {
            return apps.iter().take(limit).cloned().collect();
        }
        let min = if term.chars().count() == 1 { f64::MIN_POSITIVE } else { 0.5 };
        let mut hits: Vec<(f64, &App)> = apps.iter().map(|a| (score(term, a), a)).filter(|(s, _)| *s >= min).collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0)); // stable: ties keep scan order
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
        assert_eq!(fuzzy("", "x"), 100);
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
            best = max_opt(best, Some(sub));
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
        assert!(parse("a.desktop", "[Desktop Entry]\nName=A\nExec=a\nNoDisplay=true\n", &d).is_none());
        assert!(parse("a.desktop", "[Desktop Entry]\nName=A\nExec=a\nOnlyShowIn=GNOME;\n", &d).is_none());
        let a = parse("a.desktop", "[Desktop Entry]\nName=A\nExec=a %U\nKeywords=x;y;\n[Desktop Action n]\nName=B\n", &d).unwrap();
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
