//! Locating external programs in `$PATH`.

use std::path::{Path, PathBuf};

pub const PATH_SEP: char = if cfg!(windows) { ';' } else { ':' };

/// Does the name contain a directory part (so it isn't looked up in PATH)?
pub fn has_dir(name: &str) -> bool {
    name.contains('/') || (cfg!(windows) && (name.contains('\\') || name.contains(':')))
}

/// Find an executable. `path` is `$PATH`, `pathext` is `%PATHEXT%` (Windows).
pub fn find(name: &str, path: Option<&str>, pathext: Option<&str>, cwd: &Path) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if has_dir(name) {
        let p = crate::utils::resolve(cwd, name);
        return candidate(&p, pathext);
    }
    let path = path?;
    for dir in path.split(PATH_SEP) {
        // An empty PATH entry means the current directory.
        let dir = if dir.is_empty() { cwd.to_path_buf() } else { crate::utils::resolve(cwd, dir) };
        if let Some(p) = candidate(&dir.join(name), pathext) {
            return Some(p);
        }
    }
    None
}

/// Every match in PATH (for `where` / `which -a`).
pub fn find_all(name: &str, path: Option<&str>, pathext: Option<&str>, cwd: &Path) -> Vec<PathBuf> {
    let Some(path) = path else { return Vec::new() };
    if has_dir(name) {
        return find(name, Some(path), pathext, cwd).into_iter().collect();
    }
    path.split(PATH_SEP)
        .filter_map(|dir| {
            let dir = if dir.is_empty() { cwd.to_path_buf() } else { crate::utils::resolve(cwd, dir) };
            candidate(&dir.join(name), pathext)
        })
        .collect()
}

#[cfg(unix)]
fn candidate(p: &Path, _pathext: Option<&str>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let m = std::fs::metadata(p).ok()?;
    (m.is_file() && m.permissions().mode() & 0o111 != 0).then(|| p.to_path_buf())
}

#[cfg(not(unix))]
fn candidate(p: &Path, pathext: Option<&str>) -> Option<PathBuf> {
    let exts = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD");
    let exts: Vec<&str> = exts.split(';').filter(|e| !e.is_empty()).collect();
    let has_ext = p
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| exts.iter().any(|x| x.trim_start_matches('.').eq_ignore_ascii_case(e)));
    if has_ext && p.is_file() {
        return Some(p.to_path_buf());
    }
    for ext in &exts {
        let mut s = p.as_os_str().to_os_string();
        s.push(ext.to_ascii_lowercase());
        let q = PathBuf::from(s);
        if q.is_file() {
            return Some(q);
        }
    }
    // A script without a known extension (run through vsh if it has a shebang).
    if p.is_file() && is_shell_script(p) {
        return Some(p.to_path_buf());
    }
    None
}

/// Every executable name in PATH (for completion).
pub fn all_commands(path: Option<&str>, cwd: &Path) -> Vec<String> {
    let mut names = Vec::new();
    let Some(path) = path else { return names };
    for dir in path.split(PATH_SEP) {
        if dir.is_empty() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(crate::utils::resolve(cwd, dir)) else { continue };
        for e in rd.flatten() {
            let Ok(name) = e.file_name().into_string() else { continue };
            if cfg!(windows) {
                let lower = name.to_ascii_lowercase();
                for ext in [".exe", ".bat", ".cmd", ".com"] {
                    if let Some(stem) = lower.strip_suffix(ext) {
                        names.push(name[..stem.len()].to_string());
                    }
                }
            } else if candidate(&e.path(), None).is_some() {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Does the file start with a `#!` line naming a shell?
#[cfg_attr(unix, allow(dead_code))]
pub fn is_shell_script(p: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    let mut buf = [0u8; 128];
    let n = f.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    let Some(line) = head.strip_prefix("#!") else { return false };
    let line = line.lines().next().unwrap_or("");
    ["sh", "bash", "zsh", "vsh", "dash", "ksh"].iter().any(|s| {
        line.split(|c: char| c == '/' || c.is_whitespace()).any(|w| w == *s)
    })
}
