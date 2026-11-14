//! `ls`.

use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::Ctx;
use super::args::{self, Spec};
use super::common::{human, local_tz, msg, q, strftime, to_zoned, usage};
use super::fs::{join_disp, mode_string};

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Name,
    Time,
    Size,
    None,
}

struct Opts {
    all: bool,
    almost_all: bool,
    long: bool,
    human: bool,
    one: bool,
    recursive: bool,
    dir_itself: bool,
    sort: Sort,
    reverse: bool,
    classify: bool,
    color: bool,
    columns: bool,
    width: usize,
    numeric: bool,
}

struct Entry {
    name: String,
    path: PathBuf,
    meta: Option<Metadata>,
}

const RESET: &str = "\x1b[0m";

fn is_exec(e: &Entry) -> bool {
    let Some(m) = &e.meta else { return false };
    if !m.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = e;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        super::common::is_windows_exe(&e.path)
    }
}

fn color_of(e: &Entry) -> Option<&'static str> {
    let m = e.meta.as_ref()?;
    let ft = m.file_type();
    if ft.is_symlink() {
        return Some(if fs::metadata(&e.path).is_ok() { "\x1b[01;36m" } else { "\x1b[01;31m" });
    }
    if ft.is_dir() {
        return Some("\x1b[01;34m");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if ft.is_fifo() {
            return Some("\x1b[33m");
        }
        if ft.is_socket() {
            return Some("\x1b[01;35m");
        }
        if ft.is_block_device() || ft.is_char_device() {
            return Some("\x1b[01;33m");
        }
    }
    if is_exec(e) {
        return Some("\x1b[01;32m");
    }
    None
}

fn indicator(e: &Entry) -> &'static str {
    let Some(m) = &e.meta else { return "" };
    let ft = m.file_type();
    if ft.is_symlink() {
        return "@";
    }
    if ft.is_dir() {
        return "/";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if ft.is_fifo() {
            return "|";
        }
        if ft.is_socket() {
            return "=";
        }
    }
    if is_exec(e) { "*" } else { "" }
}

/// The decorated name and its visible width.
fn render_name(e: &Entry, o: &Opts) -> (String, usize) {
    let mut s = String::new();
    let mut w = e.name.chars().count();
    match o.color.then(|| color_of(e)).flatten() {
        Some(c) => {
            s.push_str(c);
            s.push_str(&e.name);
            s.push_str(RESET);
        }
        None => s.push_str(&e.name),
    }
    if o.classify {
        let ind = indicator(e);
        // in long format a symlink shows its target instead of '@'
        if !(o.long && ind == "@") {
            s.push_str(ind);
            w += ind.len();
        }
    }
    (s, w)
}

fn mtime(e: &Entry) -> SystemTime {
    e.meta.as_ref().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH)
}

fn sort_entries(v: &mut [Entry], o: &Opts) {
    match o.sort {
        Sort::Name => v.sort_by(|a, b| a.name.cmp(&b.name)),
        Sort::Time => v.sort_by(|a, b| mtime(b).cmp(&mtime(a)).then_with(|| a.name.cmp(&b.name))),
        Sort::Size => v.sort_by(|a, b| {
            let sa = a.meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let sb = b.meta.as_ref().map(|m| m.len()).unwrap_or(0);
            sb.cmp(&sa).then_with(|| a.name.cmp(&b.name))
        }),
        Sort::None => {}
    }
    if o.reverse && o.sort != Sort::None {
        v.reverse();
    }
}

fn blocks_1k(m: &Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (m.blocks() * 512).div_ceil(1024)
    }
    #[cfg(not(unix))]
    {
        m.len().div_ceil(1024)
    }
}

fn owner(m: &Metadata, numeric: bool) -> (String, String, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (u, g) = (m.uid(), m.gid());
        let un = if numeric { None } else { super::fs::user_name(u) };
        let gn = if numeric { None } else { super::fs::group_name(g) };
        (un.unwrap_or_else(|| u.to_string()), gn.unwrap_or_else(|| g.to_string()), m.nlink())
    }
    #[cfg(not(unix))]
    {
        let _ = (m, numeric);
        ("-".into(), "-".into(), 1)
    }
}

fn print_long(ctx: &mut Ctx, entries: &[Entry], o: &Opts, total: bool) -> io::Result<()> {
    let tz = local_tz(ctx);
    let now = SystemTime::now();
    let six_months = std::time::Duration::from_secs(60 * 60 * 24 * 182);
    struct Row {
        mode: String,
        links: String,
        user: String,
        group: String,
        size: String,
        date: String,
        name: String,
    }
    let mut rows = Vec::new();
    let mut blocks = 0;
    for e in entries {
        let Some(m) = &e.meta else { continue };
        blocks += blocks_1k(m);
        let (user, group, links) = owner(m, o.numeric);
        let size = if o.human { human(m.len()) } else { m.len().to_string() };
        let t = m.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let recent = t <= now + std::time::Duration::from_secs(3600) && now.duration_since(t).map(|d| d < six_months).unwrap_or(true);
        let date = match to_zoned(t, &tz) {
            Some(z) => strftime(&z, if recent { "%b %e %H:%M" } else { "%b %e  %Y" }),
            None => "?".into(),
        };
        let (mut name, _) = render_name(e, o);
        if m.file_type().is_symlink()
            && let Ok(t) = fs::read_link(&e.path) {
                name.push_str(" -> ");
                name.push_str(&t.to_string_lossy());
            }
        rows.push(Row { mode: mode_string(m), links: links.to_string(), user, group, size, date, name });
    }
    if total {
        writeln!(ctx.stdout, "total {blocks}")?;
    }
    let w = |f: fn(&Row) -> &String| rows.iter().map(|r| f(r).chars().count()).max().unwrap_or(0);
    let (wl, wu, wg, ws) = (w(|r| &r.links), w(|r| &r.user), w(|r| &r.group), w(|r| &r.size));
    for r in &rows {
        writeln!(
            ctx.stdout,
            "{} {:>wl$} {:<wu$} {:<wg$} {:>ws$} {} {}",
            r.mode, r.links, r.user, r.group, r.size, r.date, r.name
        )?;
    }
    Ok(())
}

fn print_names(ctx: &mut Ctx, entries: &[Entry], o: &Opts) -> io::Result<()> {
    let items: Vec<(String, usize)> = entries.iter().map(|e| render_name(e, o)).collect();
    if items.is_empty() {
        return Ok(());
    }
    if o.one || !o.columns {
        for (s, _) in &items {
            writeln!(ctx.stdout, "{s}")?;
        }
        return Ok(());
    }
    // Vertical column layout: as many columns as fit the width.
    let n = items.len();
    let mut best: (usize, Vec<usize>) = (n, vec![items.iter().map(|i| i.1).max().unwrap_or(0)]);
    for cols in (2..=n.min(o.width / 3 + 1)).rev() {
        let rows = n.div_ceil(cols);
        let cols_used = n.div_ceil(rows);
        let mut widths = vec![0; cols_used];
        for (i, it) in items.iter().enumerate() {
            let c = i / rows;
            widths[c] = widths[c].max(it.1);
        }
        let total: usize = widths.iter().sum::<usize>() + 2 * (cols_used - 1);
        if total <= o.width {
            best = (rows, widths);
            break;
        }
    }
    let (rows, widths) = best;
    for r in 0..rows {
        let mut line = String::new();
        let mut c = 0;
        while r + c * rows < n {
            let (s, w) = &items[r + c * rows];
            line.push_str(s);
            if r + (c + 1) * rows < n {
                line.push_str(&" ".repeat(widths[c] - w + 2));
            }
            c += 1;
        }
        writeln!(ctx.stdout, "{line}")?;
    }
    Ok(())
}

fn print_entries(ctx: &mut Ctx, entries: &[Entry], o: &Opts, total: bool) -> io::Result<()> {
    if o.long { print_long(ctx, entries, o, total) } else { print_names(ctx, entries, o) }
}

fn read_dir_entries(dir: &Path, o: &Opts) -> io::Result<Vec<Entry>> {
    let mut v = Vec::new();
    if o.all {
        for n in [".", ".."] {
            let p = dir.join(n);
            v.push(Entry { name: n.into(), meta: fs::metadata(&p).ok(), path: p });
        }
    }
    for e in fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !o.all && !o.almost_all {
            continue;
        }
        let path = e.path();
        v.push(Entry { name, meta: fs::symlink_metadata(&path).ok(), path });
    }
    sort_entries(&mut v, o);
    Ok(v)
}

fn list_dir(ctx: &mut Ctx, dir: &Path, disp: &str, o: &Opts, header: bool, first: &mut bool) -> io::Result<bool> {
    ctx.check_interrupt()?;
    let entries = match read_dir_entries(dir, o) {
        Ok(v) => v,
        Err(e) => {
            ctx.err(format!("cannot open directory {}: {}", q(disp), msg(&e)));
            return Ok(false);
        }
    };
    if header {
        if !*first {
            writeln!(ctx.stdout)?;
        }
        writeln!(ctx.stdout, "{disp}:")?;
    }
    *first = false;
    print_entries(ctx, &entries, o, true)?;
    let mut ok = true;
    if o.recursive {
        for e in &entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            if e.meta.as_ref().is_some_and(|m| m.is_dir()) {
                ok &= list_dir(ctx, &e.path, &join_disp(disp, &e.name), o, true, first)?;
            }
        }
    }
    Ok(ok)
}

pub fn ls(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "w", long_with_value: &["width"], allow_number: false, stop_at_operand: false };
    let p = match args::parse(args, &spec) {
        Ok(p) => p,
        Err(e) => return usage(ctx, e, 2),
    };
    let longs = ["all", "almost-all", "human-readable", "recursive", "directory", "reverse", "classify", "color", "colour", "width", "numeric-uid-gid"];
    if let Some(e) = p.unknown("aAlh1RdtSrFUCnGw", &longs) {
        return usage(ctx, e, 2);
    }
    let color = match p.long_value("color").or(p.long_value("colour")) {
        Some("always" | "yes" | "force") => true,
        Some("never" | "no" | "none") => false,
        Some("auto" | "tty" | "if-tty") => ctx.stdout_tty,
        None if p.long("color") || p.long("colour") => true,
        None => ctx.stdout_tty || p.flag('G'),
        Some(other) => return usage(ctx, format!("invalid argument {} for '--color'", q(other)), 2),
    };
    let width = p
        .value('w')
        .or(p.long_value("width"))
        .and_then(|w| w.parse().ok())
        .or_else(|| terminal_size::terminal_size().map(|(w, _)| w.0 as usize).filter(|_| ctx.stdout_tty))
        .or_else(|| ctx.getenv("COLUMNS").and_then(|c| c.parse().ok()))
        .unwrap_or(80);
    let o = Opts {
        all: p.flag('a') || p.long("all"),
        almost_all: p.flag('A') || p.long("almost-all"),
        long: p.flag('l') || p.flag('n') || p.long("numeric-uid-gid"),
        human: p.flag('h') || p.long("human-readable"),
        one: p.flag('1'),
        recursive: p.flag('R') || p.long("recursive"),
        dir_itself: p.flag('d') || p.long("directory"),
        sort: if p.flag('U') {
            Sort::None
        } else if p.flag('t') {
            Sort::Time
        } else if p.flag('S') {
            Sort::Size
        } else {
            Sort::Name
        },
        reverse: p.flag('r') || p.long("reverse"),
        classify: p.flag('F') || p.long("classify"),
        color,
        columns: ctx.stdout_tty || p.flag('C'),
        width,
        numeric: p.flag('n') || p.long("numeric-uid-gid"),
    };
    let ops = if p.operands.is_empty() { vec![".".to_string()] } else { p.operands.clone() };
    let mut status = 0;
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for n in &ops {
        let path = ctx.path(n);
        let lmeta = fs::symlink_metadata(&path);
        match lmeta {
            Err(e) => {
                ctx.err(format!("cannot access {}: {}", q(n), msg(&e)));
                status = 2;
            }
            Ok(lm) => {
                // command-line symlinks to directories are followed unless -l/-d
                let is_dir = if lm.file_type().is_symlink() && !o.long {
                    fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false)
                } else {
                    lm.is_dir()
                };
                let e = Entry { name: n.clone(), path, meta: Some(lm) };
                if is_dir && !o.dir_itself { dirs.push(e) } else { files.push(e) }
            }
        }
    }
    sort_entries(&mut files, &o);
    sort_entries(&mut dirs, &o);
    print_entries(ctx, &files, &o, false)?;
    let header = ops.len() > 1 || o.recursive;
    let mut first = files.is_empty();
    for d in &dirs {
        if !list_dir(ctx, &d.path, &d.name, &o, header, &mut first)? && status == 0 {
            status = 1;
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use crate::utils::common::testutil::TempDir;
    use crate::utils::run_capture;

    fn setup() -> TempDir {
        let d = TempDir::new("ls");
        d.write("b.txt", "12345");
        d.write("a.txt", "1");
        d.write(".hidden", "");
        d.write("sub/inner", "zz");
        d
    }

    #[test]
    fn plain_and_flags() {
        let d = setup();
        let r = |a: &[&str]| run_capture("ls", a, b"", d.path());
        assert_eq!(r(&[]).1, "a.txt\nb.txt\nsub\n");
        assert_eq!(r(&["-a"]).1, ".\n..\n.hidden\na.txt\nb.txt\nsub\n");
        assert_eq!(r(&["-A"]).1, ".hidden\na.txt\nb.txt\nsub\n");
        assert_eq!(r(&["-r"]).1, "sub\nb.txt\na.txt\n");
        assert_eq!(r(&["-S", "a.txt", "b.txt"]).1, "b.txt\na.txt\n");
        assert_eq!(r(&["-F"]).1, "a.txt\nb.txt\nsub/\n");
        assert_eq!(r(&["-d", "sub"]).1, "sub\n");
        assert_eq!(r(&["sub"]).1, "inner\n");
        assert_eq!(r(&["a.txt", "sub"]).1, "a.txt\n\nsub:\ninner\n");
        assert_eq!(r(&["-R"]).1, ".:\na.txt\nb.txt\nsub\n\n./sub:\ninner\n");
        let (s, _, e) = r(&["nope"]);
        assert_eq!((s, e.as_str()), (2, "ls: cannot access 'nope': No such file or directory\n"));
        assert_eq!(r(&["--color=always", "sub"]).1, "inner\n");
        assert!(r(&["--color=always"]).1.contains("\x1b[01;34msub\x1b[0m"));
        assert_eq!(r(&["-C", "-w", "80"]).1, "a.txt  b.txt  sub\n");
        assert_eq!(r(&["-C", "-w", "12"]).1, "a.txt  sub\nb.txt\n");
        assert_eq!(r(&["-Q"]).0, 2);
    }

    #[test]
    fn long_format() {
        let d = setup();
        let (s, out, _) = run_capture("ls", &["-l"], b"", d.path());
        assert_eq!(s, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("total "));
        assert_eq!(lines.len(), 4);
        assert!(lines[1].starts_with('-') && lines[1].ends_with(" a.txt"), "{out}");
        assert!(lines[3].starts_with('d') && lines[3].ends_with(" sub"), "{out}");
        assert!(lines[2].contains(" 5 "), "{out}");
        let (_, out, _) = run_capture("ls", &["-lh", "b.txt"], b"", d.path());
        assert!(!out.starts_with("total"));
    }
}
