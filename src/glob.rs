//! Pattern matching and filename generation.
//!
//! Patterns are strings in which `\x` means a literal `x` (expansion escapes
//! quoted text that way), `*`, `?` and `[...]` are wildcards, and a `**/`
//! path component matches any number of directories (zsh).

use std::path::Path;

#[derive(Debug, Clone)]
enum Tok {
    Char(char),
    Any,
    Star,
    Class { negated: bool, items: Vec<ClassItem> },
}

#[derive(Debug, Clone)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Named(String),
}

fn tokenize(pat: &str) -> Vec<Tok> {
    let chars: Vec<char> = pat.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '\\' if i < chars.len() => {
                toks.push(Tok::Char(chars[i]));
                i += 1;
            }
            '*' => {
                if !matches!(toks.last(), Some(Tok::Star)) {
                    toks.push(Tok::Star);
                }
            }
            '?' => toks.push(Tok::Any),
            '[' => match parse_class(&chars, i) {
                Some((tok, next)) => {
                    toks.push(tok);
                    i = next;
                }
                None => toks.push(Tok::Char('[')),
            },
            c => toks.push(Tok::Char(c)),
        }
    }
    toks
}

/// Parse a bracket expression starting after `[`. Returns the token and the
/// index after the closing `]`, or `None` if it isn't closed.
fn parse_class(chars: &[char], mut i: usize) -> Option<(Tok, usize)> {
    let mut negated = false;
    if i < chars.len() && (chars[i] == '!' || chars[i] == '^') {
        negated = true;
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while i < chars.len() {
        let c = chars[i];
        if c == ']' && !first {
            return Some((Tok::Class { negated, items }, i + 1));
        }
        first = false;
        if c == '[' && chars.get(i + 1) == Some(&':') {
            let rest: String = chars[i + 2..].iter().collect();
            if let Some(end) = rest.find(":]") {
                items.push(ClassItem::Named(rest[..end].to_string()));
                i += 2 + rest[..end].chars().count() + 2;
                continue;
            }
        }
        let lo = if c == '\\' && i + 1 < chars.len() {
            i += 1;
            chars[i]
        } else {
            c
        };
        i += 1;
        if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(|&n| n != ']') {
            let mut hi = chars[i + 1];
            i += 2;
            if hi == '\\' && i < chars.len() {
                hi = chars[i];
                i += 1;
            }
            items.push(ClassItem::Range(lo, hi));
        } else {
            items.push(ClassItem::Char(lo));
        }
    }
    None
}

fn class_matches(negated: bool, items: &[ClassItem], c: char, fold: bool) -> bool {
    let test = |c: char| {
        items.iter().any(|it| match it {
            ClassItem::Char(x) => *x == c,
            ClassItem::Range(a, b) => *a <= c && c <= *b,
            ClassItem::Named(n) => match n.as_str() {
                "alpha" => c.is_alphabetic(),
                "digit" => c.is_ascii_digit(),
                "alnum" => c.is_alphanumeric(),
                "upper" => c.is_uppercase(),
                "lower" => c.is_lowercase(),
                "space" => c.is_whitespace(),
                "blank" => c == ' ' || c == '\t',
                "punct" => c.is_ascii_punctuation(),
                "xdigit" => c.is_ascii_hexdigit(),
                "cntrl" => c.is_control(),
                "print" => !c.is_control(),
                "graph" => !c.is_control() && !c.is_whitespace(),
                _ => false,
            },
        })
    };
    let m = test(c) || (fold && (test(c.to_ascii_lowercase()) || test(c.to_ascii_uppercase())));
    m != negated
}

fn tok_matches(t: &Tok, c: char, fold: bool) -> bool {
    match t {
        Tok::Char(x) => *x == c || (fold && x.eq_ignore_ascii_case(&c)),
        Tok::Any => true,
        Tok::Class { negated, items } => class_matches(*negated, items, c, fold),
        Tok::Star => unreachable!(),
    }
}

fn match_toks(toks: &[Tok], s: &[char], fold: bool) -> bool {
    // Classic wildcard matching with backtracking to the last star.
    let (mut p, mut i) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while i < s.len() {
        if p < toks.len() && !matches!(toks[p], Tok::Star) && tok_matches(&toks[p], s[i], fold) {
            p += 1;
            i += 1;
        } else if p < toks.len() && matches!(toks[p], Tok::Star) {
            star = Some((p, i));
            p += 1;
        } else if let Some((sp, si)) = star {
            p = sp + 1;
            i = si + 1;
            star = Some((sp, si + 1));
        } else {
            return false;
        }
    }
    while p < toks.len() && matches!(toks[p], Tok::Star) {
        p += 1;
    }
    p == toks.len()
}

/// Does the whole string match the pattern?
pub fn matches(pat: &str, s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    match_toks(&tokenize(pat), &chars, false)
}

/// Does the pattern contain an unescaped wildcard?
pub fn has_wildcards(pat: &str) -> bool {
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '*' | '?' => return true,
            '[' if parse_class(&chars, i + 1).is_some() => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Escape glob metacharacters so the text matches literally.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Remove pattern escapes.
pub fn unescape(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len());
    let mut it = pat.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Byte offsets where `s` can be split; used by `${x#pat}` and friends.
fn boundaries(s: &str) -> Vec<usize> {
    s.char_indices().map(|(i, _)| i).chain(std::iter::once(s.len())).collect()
}

/// Remove the shortest/longest matching prefix.
pub fn trim_prefix(s: &str, pat: &str, longest: bool) -> String {
    let b = boundaries(s);
    let it: Box<dyn Iterator<Item = &usize>> = if longest { Box::new(b.iter().rev()) } else { Box::new(b.iter()) };
    for &i in it {
        if matches(pat, &s[..i]) {
            return s[i..].to_string();
        }
    }
    s.to_string()
}

pub fn trim_suffix(s: &str, pat: &str, longest: bool) -> String {
    let b = boundaries(s);
    let it: Box<dyn Iterator<Item = &usize>> = if longest { Box::new(b.iter()) } else { Box::new(b.iter().rev()) };
    for &i in it {
        if matches(pat, &s[i..]) {
            return s[..i].to_string();
        }
    }
    s.to_string()
}

/// `${x/pat/rep}`: replace the first (or every) longest match.
pub fn replace(s: &str, pat: &str, rep: &str, all: bool, anchor: Option<char>) -> String {
    let b = boundaries(s);
    match anchor {
        Some('#') => {
            for &j in b.iter().rev() {
                if matches(pat, &s[..j]) {
                    return format!("{rep}{}", &s[j..]);
                }
            }
            return s.to_string();
        }
        Some('%') => {
            for &i in &b {
                if matches(pat, &s[i..]) {
                    return format!("{}{rep}", &s[..i]);
                }
            }
            return s.to_string();
        }
        _ => {}
    }
    let mut out = String::new();
    let mut k = 0;
    while k < b.len() {
        let i = b[k];
        let found = b[k..].iter().rev().find(|&&j| j > i && matches(pat, &s[i..j]));
        match found {
            Some(&j) => {
                out.push_str(rep);
                if !all {
                    out.push_str(&s[j..]);
                    return out;
                }
                k = b.iter().position(|&x| x == j).unwrap();
            }
            None => {
                if i < s.len() {
                    out.push_str(&s[i..b[k + 1]]);
                }
                k += 1;
            }
        }
    }
    out
}

/// Expand a filename pattern relative to `cwd`. Returns sorted matches, as
/// paths written the way the pattern was (relative stays relative).
pub fn glob(pat: &str, cwd: &Path, dotglob: bool) -> Vec<String> {
    let fold = cfg!(windows);
    let (root, rest) = split_root(pat);
    let comps: Vec<&str> = rest.split('/').collect();
    let mut results = Vec::new();
    let start = if root.is_empty() { String::new() } else { unescape(&root) };
    walk(&start, &comps, cwd, dotglob, fold, &mut results);
    results.sort();
    results.dedup();
    results
}

fn split_root(pat: &str) -> (String, &str) {
    if let Some(rest) = pat.strip_prefix('/') {
        return ("/".into(), rest);
    }
    // Windows drive: C:/...
    let b = pat.as_bytes();
    if cfg!(windows) && b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\') {
        return (pat[..3].replace('\\', "/"), &pat[3..]);
    }
    (String::new(), pat)
}

fn walk(prefix: &str, comps: &[&str], cwd: &Path, dotglob: bool, fold: bool, out: &mut Vec<String>) {
    let Some((&comp, rest)) = comps.split_first() else {
        out.push(prefix.trim_end_matches('/').to_string().if_empty("/"));
        return;
    };
    let join = |name: &str| -> String {
        if prefix.is_empty() {
            name.to_string()
        } else if prefix.ends_with('/') {
            format!("{prefix}{name}")
        } else {
            format!("{prefix}/{name}")
        }
    };
    let dir = if prefix.is_empty() { cwd.to_path_buf() } else { cwd.join(prefix) };
    if comp.is_empty() {
        // Trailing slash or `//`: only directories continue.
        if rest.is_empty() {
            if dir.is_dir() {
                out.push(format!("{}/", prefix.trim_end_matches('/')));
            }
        } else {
            walk(prefix, rest, cwd, dotglob, fold, out);
        }
        return;
    }
    if comp == "**" && !rest.is_empty() {
        // Zero directories...
        walk(prefix, rest, cwd, dotglob, fold, out);
        // ...or one more, recursively.
        for name in list_dir(&dir) {
            if !dotglob && name.starts_with('.') {
                continue;
            }
            let p = dir.join(&name);
            let is_dir = std::fs::symlink_metadata(&p).map(|m| m.is_dir()).unwrap_or(false);
            if is_dir {
                walk(&join(&name), comps, cwd, dotglob, fold, out);
            }
        }
        return;
    }
    if !has_wildcards(comp) {
        let name = unescape(comp);
        let next = join(&name);
        let p = cwd.join(&next);
        if rest.is_empty() {
            if std::fs::symlink_metadata(&p).is_ok() {
                out.push(next);
            }
        } else if p.is_dir() {
            walk(&next, rest, cwd, dotglob, fold, out);
        }
        return;
    }
    let toks = tokenize(comp);
    let explicit_dot = comp.starts_with('.') || comp.starts_with("\\.");
    for name in list_dir(&dir) {
        if name.starts_with('.') && !explicit_dot && !dotglob {
            continue;
        }
        let chars: Vec<char> = name.chars().collect();
        if !match_toks(&toks, &chars, fold) {
            continue;
        }
        let next = join(&name);
        if rest.is_empty() {
            out.push(next);
        } else if dir.join(&name).is_dir() {
            walk(&next, rest, cwd, dotglob, fold, out);
        }
    }
}

fn list_dir(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    names
}

trait IfEmpty {
    fn if_empty(self, other: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, other: &str) -> String {
        if self.is_empty() { other.to_string() } else { self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matching() {
        assert!(matches("*.rs", "main.rs"));
        assert!(!matches("*.rs", "main.rc"));
        assert!(matches("a*b*c", "aXXbYYc"));
        assert!(matches("a?c", "abc"));
        assert!(matches("[a-c]x", "bx"));
        assert!(!matches("[!a-c]x", "bx"));
        assert!(matches("[[:digit:]]*", "9lives"));
        assert!(matches("\\*", "*"));
        assert!(!matches("\\*", "x"));
        assert!(matches("*", ""));
        assert!(matches("[]]", "]"));
        assert!(matches("[", "["));
    }

    #[test]
    fn trims_and_replace() {
        assert_eq!(trim_prefix("a/b/c", "*/", false), "b/c");
        assert_eq!(trim_prefix("a/b/c", "*/", true), "c");
        assert_eq!(trim_suffix("f.tar.gz", ".*", false), "f.tar");
        assert_eq!(trim_suffix("f.tar.gz", ".*", true), "f");
        assert_eq!(replace("hello", "l", "L", false, None), "heLlo");
        assert_eq!(replace("hello", "l", "L", true, None), "heLLo");
        assert_eq!(replace("hello", "h*", "X", false, Some('#')), "X");
        assert_eq!(replace("hello", "lo", "P", false, Some('%')), "helP");
    }

    #[test]
    fn filesystem_glob() {
        let d = std::env::temp_dir().join(format!("vsh-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub/deep")).unwrap();
        for f in ["a.txt", "b.txt", ".hidden.txt", "sub/c.txt", "sub/deep/d.txt"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        assert_eq!(glob("*.txt", &d, false), ["a.txt", "b.txt"]);
        assert_eq!(glob(".*.txt", &d, false), [".hidden.txt"]);
        assert_eq!(glob("*/*.txt", &d, false), ["sub/c.txt"]);
        assert_eq!(glob("**/*.txt", &d, false), ["a.txt", "b.txt", "sub/c.txt", "sub/deep/d.txt"]);
        assert_eq!(glob("s*/", &d, false), ["sub/"]);
        assert!(glob("*.nope", &d, false).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
