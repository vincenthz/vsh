//! File and directory utilities.

use std::fs::{self, File, Metadata};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::Ctx;
use super::args::{self, Spec};
use super::common::{display, human, local_tz, msg, q, random_u64, strftime, to_zoned, usage};

// ---------------------------------------------------------------------------
// shared helpers

fn is_sep(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// Join a display path the way the user would type it.
pub fn join_disp(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else if base.ends_with(is_sep) {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// The last component of a path string (GNU basename without suffix).
pub fn base_name(s: &str) -> String {
    let t = s.trim_end_matches(is_sep);
    if t.is_empty() {
        return if s.is_empty() { String::new() } else { "/".into() };
    }
    match t.rfind(is_sep) {
        Some(i) => t[i + 1..].to_string(),
        None => t.to_string(),
    }
}

pub fn dir_name(s: &str) -> String {
    let t = s.trim_end_matches(is_sep);
    if t.is_empty() {
        return if s.is_empty() { ".".into() } else { "/".into() };
    }
    match t.rfind(is_sep) {
        Some(i) => {
            let d = t[..i].trim_end_matches(is_sep);
            if d.is_empty() {
                "/".into()
            } else if cfg!(windows) && d.len() == 2 && d.ends_with(':') {
                format!("{d}/")
            } else {
                d.to_string()
            }
        }
        None => ".".into(),
    }
}

#[cfg(unix)]
pub fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut res) };
    if r != 0 || res.is_null() {
        return None;
    }
    let c = unsafe { std::ffi::CStr::from_ptr(pw.pw_name) };
    Some(c.to_string_lossy().into_owned())
}

#[cfg(unix)]
pub fn group_name(gid: u32) -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    let mut res: *mut libc::group = std::ptr::null_mut();
    let r = unsafe { libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut res) };
    if r != 0 || res.is_null() {
        return None;
    }
    let c = unsafe { std::ffi::CStr::from_ptr(gr.gr_name) };
    Some(c.to_string_lossy().into_owned())
}

#[cfg(unix)]
pub fn perm_string(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    let bits = [(0o400, 'r'), (0o200, 'w'), (0o100, 'x'), (0o040, 'r'), (0o020, 'w'), (0o010, 'x'), (0o004, 'r'), (0o002, 'w'), (0o001, 'x')];
    for (i, (b, c)) in bits.iter().enumerate() {
        let set = mode & b != 0;
        let ch = match i {
            2 if mode & 0o4000 != 0 => if set { 's' } else { 'S' },
            5 if mode & 0o2000 != 0 => if set { 's' } else { 'S' },
            8 if mode & 0o1000 != 0 => if set { 't' } else { 'T' },
            _ => if set { *c } else { '-' },
        };
        s.push(ch);
    }
    s
}

/// `ls -l` style mode, e.g. `drwxr-xr-x`.
pub fn mode_string(m: &Metadata) -> String {
    let ft = m.file_type();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        let t = if ft.is_dir() {
            'd'
        } else if ft.is_symlink() {
            'l'
        } else if ft.is_char_device() {
            'c'
        } else if ft.is_block_device() {
            'b'
        } else if ft.is_fifo() {
            'p'
        } else if ft.is_socket() {
            's'
        } else {
            '-'
        };
        format!("{t}{}", perm_string(m.permissions().mode()))
    }
    #[cfg(not(unix))]
    {
        let t = if ft.is_symlink() { 'l' } else if ft.is_dir() { 'd' } else { '-' };
        let w = if m.permissions().readonly() { '-' } else { 'w' };
        let x = if ft.is_dir() { 'x' } else { '-' };
        let p = format!("r{w}{x}");
        format!("{t}{p}{p}{p}")
    }
}

pub fn file_type_name(m: &Metadata) -> &'static str {
    let ft = m.file_type();
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if ft.is_char_device() {
            return "character special file";
        }
        if ft.is_block_device() {
            return "block special file";
        }
        if ft.is_fifo() {
            return "fifo";
        }
        if ft.is_socket() {
            return "socket";
        }
    }
    if ft.is_symlink() {
        "symbolic link"
    } else if ft.is_dir() {
        "directory"
    } else if m.len() == 0 {
        "regular empty file"
    } else {
        "regular file"
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

fn sorted_entries(dir: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut v: Vec<fs::DirEntry> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    v.sort_by_key(|e| e.file_name());
    Ok(v)
}

/// Resolve a path like `readlink -f`: every component but the last must exist.
fn canonicalize_loose(p: &Path, need_all: bool, allow_missing: bool) -> io::Result<PathBuf> {
    match fs::canonicalize(p) {
        Ok(c) => Ok(c),
        Err(e) if need_all => Err(e),
        Err(e) => {
            if allow_missing {
                // canonicalize the longest existing prefix, append the rest
                let mut rest = Vec::new();
                let mut cur = p.to_path_buf();
                loop {
                    if let Ok(c) = fs::canonicalize(&cur) {
                        let mut out = c;
                        for r in rest.iter().rev() {
                            out.push(r);
                        }
                        return Ok(out);
                    }
                    match (cur.file_name().map(|s| s.to_os_string()), cur.parent()) {
                        (Some(n), Some(par)) => {
                            rest.push(n);
                            cur = par.to_path_buf();
                        }
                        _ => return Err(e),
                    }
                }
            }
            let (Some(name), Some(parent)) = (p.file_name(), p.parent()) else { return Err(e) };
            let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
            Ok(fs::canonicalize(parent)?.join(name))
        }
    }
}

fn parse_or(ctx: &mut Ctx, args: &[String], spec: &Spec, shorts: &str, longs: &[&str]) -> Result<args::Opts, i32> {
    match args::parse(args, spec) {
        Ok(o) => match o.unknown(shorts, longs) {
            Some(e) => {
                ctx.err(e);
                Err(1)
            }
            None => Ok(o),
        },
        Err(e) => {
            ctx.err(e);
            Err(1)
        }
    }
}

macro_rules! opts {
    ($ctx:expr, $args:expr, $spec:expr, $shorts:expr, $longs:expr) => {
        match parse_or($ctx, $args, $spec, $shorts, $longs) {
            Ok(o) => o,
            Err(s) => return Ok(s),
        }
    };
}

// ---------------------------------------------------------------------------
// basename, dirname, realpath, readlink

pub fn basename(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "s", long_with_value: &["suffix"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "asz", &["multiple", "suffix", "zero"]);
    let suffix = o.value('s').or(o.long_value("suffix")).map(String::from);
    let multiple = o.flag('a') || o.long("multiple") || suffix.is_some();
    let end = if o.flag('z') { "\0" } else { "\n" };
    let (names, suffix): (Vec<&String>, Option<String>) = if multiple {
        (o.operands.iter().collect(), suffix)
    } else {
        match o.operands.len() {
            0 => return usage(ctx, "missing operand", 1),
            1 => (vec![&o.operands[0]], None),
            2 => (vec![&o.operands[0]], Some(o.operands[1].clone())),
            _ => return usage(ctx, format!("extra operand {}", q(&o.operands[2])), 1),
        }
    };
    if names.is_empty() {
        return usage(ctx, "missing operand", 1);
    }
    for n in names {
        let mut b = base_name(n);
        if let Some(s) = &suffix
            && !s.is_empty() && b.len() > s.len() && b.ends_with(s.as_str()) {
                b.truncate(b.len() - s.len());
            }
        write!(ctx.stdout, "{b}{end}")?;
    }
    Ok(0)
}

pub fn dirname(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "z", &["zero"]);
    if o.operands.is_empty() {
        return usage(ctx, "missing operand", 1);
    }
    let end = if o.flag('z') { "\0" } else { "\n" };
    for n in &o.operands {
        write!(ctx.stdout, "{}{end}", dir_name(n))?;
    }
    Ok(0)
}

pub fn realpath(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "emsqz", &["canonicalize-existing", "canonicalize-missing", "strip", "no-symlinks", "quiet", "zero"]);
    if o.operands.is_empty() {
        return usage(ctx, "missing operand", 1);
    }
    let end = if o.flag('z') { "\0" } else { "\n" };
    let mut status = 0;
    for n in &o.operands {
        let p = ctx.path(n);
        let r = if o.flag('s') || o.long("strip") || o.long("no-symlinks") {
            Ok(lexical_normalize(&p))
        } else {
            canonicalize_loose(&p, o.flag('e') || o.long("canonicalize-existing"), o.flag('m') || o.long("canonicalize-missing"))
        };
        match r {
            Ok(c) => write!(ctx.stdout, "{}{end}", display(&c))?,
            Err(e) => {
                if !o.flag('q') {
                    ctx.err(format!("{n}: {}", msg(&e)));
                }
                status = 1;
            }
        }
    }
    Ok(status)
}

fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn readlink(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "femnqsvz", &["canonicalize", "canonicalize-existing", "canonicalize-missing", "no-newline", "quiet", "silent", "verbose", "zero"]);
    if o.operands.is_empty() {
        return usage(ctx, "missing operand", 1);
    }
    let end = if o.flag('z') { "\0" } else if o.flag('n') && o.operands.len() == 1 { "" } else { "\n" };
    let canon = o.flag('f') || o.flag('e') || o.flag('m') || o.long("canonicalize") || o.long("canonicalize-existing") || o.long("canonicalize-missing");
    let mut status = 0;
    for n in &o.operands {
        let p = ctx.path(n);
        let r = if canon {
            canonicalize_loose(&p, o.flag('e') || o.long("canonicalize-existing"), o.flag('m') || o.long("canonicalize-missing"))
        } else {
            fs::read_link(&p)
        };
        match r {
            Ok(t) => write!(ctx.stdout, "{}{end}", display(&t))?,
            Err(e) => {
                if o.flag('v') {
                    ctx.err(format!("{n}: {}", msg(&e)));
                }
                status = 1;
            }
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// mkdir, rmdir, touch

#[cfg(unix)]
fn parse_mode_for_mkdir(s: &str) -> Option<u32> {
    parse_mode(s, 0o777, true)
}

pub fn mkdir(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "m", long_with_value: &["mode"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "pvm", &["parents", "verbose", "mode"]);
    if o.operands.is_empty() {
        return usage(ctx, "missing operand", 1);
    }
    let parents = o.flag('p') || o.long("parents");
    let verbose = o.flag('v') || o.long("verbose");
    #[cfg(unix)]
    let mode = match o.value('m').or(o.long_value("mode")) {
        Some(m) => match parse_mode_for_mkdir(m) {
            Some(v) => Some(v),
            None => return usage(ctx, format!("invalid mode {}", q(m)), 1),
        },
        None => None,
    };
    #[cfg(not(unix))]
    let _ = o.value('m');
    let mut status = 0;
    for n in &o.operands {
        let target = ctx.path(n);
        let mut steps: Vec<(PathBuf, String)> = Vec::new();
        if parents {
            // every prefix of the operand, so messages show what was typed
            let mut acc = String::new();
            for (i, part) in n.split(is_sep).enumerate() {
                if i > 0 {
                    acc.push('/');
                }
                acc.push_str(part);
                if part.is_empty() || part == "." {
                    continue;
                }
                steps.push((ctx.path(&acc), acc.clone()));
            }
        } else {
            steps.push((target.clone(), n.clone()));
        }
        for (p, disp) in steps {
            if parents && p.is_dir() {
                continue;
            }
            match fs::create_dir(&p) {
                Ok(()) => {
                    if verbose {
                        writeln!(ctx.stdout, "mkdir: created directory {}", q(&disp))?;
                    }
                }
                Err(e) => {
                    ctx.err(format!("cannot create directory {}: {}", q(&disp), msg(&e)));
                    status = 1;
                    break;
                }
            }
        }
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            if target.is_dir()
                && let Err(e) = fs::set_permissions(&target, fs::Permissions::from_mode(m)) {
                    ctx.err(format!("{}: {}", q(n), msg(&e)));
                    status = 1;
                }
        }
    }
    Ok(status)
}

pub fn rmdir(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "pv", &["parents", "verbose", "ignore-fail-on-non-empty"]);
    if o.operands.is_empty() {
        return usage(ctx, "missing operand", 1);
    }
    let ignore_ne = o.long("ignore-fail-on-non-empty");
    let mut status = 0;
    for n in &o.operands {
        let mut cur = n.trim_end_matches(is_sep).to_string();
        loop {
            if o.flag('v') || o.long("verbose") {
                writeln!(ctx.stdout, "rmdir: removing directory, {}", q(&cur))?;
            }
            if let Err(e) = fs::remove_dir(ctx.path(&cur)) {
                let not_empty = e.kind() == ErrorKind::DirectoryNotEmpty;
                if !(ignore_ne && not_empty) {
                    ctx.err(format!("failed to remove {}: {}", q(&cur), msg(&e)));
                    status = 1;
                }
                break;
            }
            if !(o.flag('p') || o.long("parents")) {
                break;
            }
            let d = dir_name(&cur);
            if d == "." || d == "/" || d == cur {
                break;
            }
            cur = d;
        }
    }
    Ok(status)
}

fn open_for_times(p: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        File::open(p)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS lets us open directories.
        fs::OpenOptions::new().write(true).custom_flags(0x0200_0000).open(p)
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::OpenOptions::new().write(true).open(p)
    }
}

pub fn touch(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "dr", long_with_value: &["date", "reference"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "acmdrh", &["no-create", "date", "reference", "no-dereference"]);
    if o.operands.is_empty() {
        return usage(ctx, "missing file operand", 1);
    }
    let when: SystemTime = if let Some(d) = o.value('d').or(o.long_value("date")) {
        let tz = local_tz(ctx);
        match super::misc::parse_date(d, &tz) {
            Some(z) => z.timestamp().into(),
            None => return usage(ctx, format!("invalid date format {}", q(d)), 1),
        }
    } else if let Some(r) = o.value('r').or(o.long_value("reference")) {
        match fs::metadata(ctx.path(r)).and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) => return usage(ctx, format!("failed to get attributes of {}: {}", q(r), msg(&e)), 1),
        }
    } else {
        SystemTime::now()
    };
    let only_a = o.flag('a') && !o.flag('m');
    let only_m = o.flag('m') && !o.flag('a');
    let mut status = 0;
    for n in &o.operands {
        let p = ctx.path(n);
        if fs::symlink_metadata(&p).is_err() {
            if o.flag('c') || o.long("no-create") {
                continue;
            }
            if let Err(e) = fs::OpenOptions::new().create(true).append(true).open(&p) {
                ctx.err(format!("cannot touch {}: {}", q(n), msg(&e)));
                status = 1;
                continue;
            }
        }
        let mut times = fs::FileTimes::new();
        if !only_m {
            times = times.set_accessed(when);
        }
        if !only_a {
            times = times.set_modified(when);
        }
        if let Err(e) = open_for_times(&p).and_then(|f| f.set_times(times)) {
            ctx.err(format!("setting times of {}: {}", q(n), msg(&e)));
            status = 1;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// rm

struct RmOpts {
    recursive: bool,
    force: bool,
    verbose: bool,
    dir: bool,
}

fn remove_file_force(p: &Path, force: bool) -> io::Result<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) => {
            // a directory symlink on Windows is removed with remove_dir
            if cfg!(windows) && fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false)
                && fs::remove_dir(p).is_ok() {
                    return Ok(());
                }
            if force && e.kind() == ErrorKind::PermissionDenied
                && let Ok(m) = fs::symlink_metadata(p) {
                    let mut perm = m.permissions();
                    if perm.readonly() {
                        #[allow(clippy::permissions_set_readonly_false)]
                        perm.set_readonly(false);
                        if fs::set_permissions(p, perm).is_ok() {
                            return fs::remove_file(p);
                        }
                    }
                }
            Err(e)
        }
    }
}

fn rm_path(ctx: &mut Ctx, p: &Path, disp: &str, o: &RmOpts) -> io::Result<bool> {
    ctx.check_interrupt()?;
    let m = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) => {
            if o.force && e.kind() == ErrorKind::NotFound {
                return Ok(true);
            }
            ctx.err(format!("cannot remove {}: {}", q(disp), msg(&e)));
            return Ok(false);
        }
    };
    if m.is_dir() {
        if !o.recursive {
            if o.dir {
                return match fs::remove_dir(p) {
                    Ok(()) => {
                        if o.verbose {
                            writeln!(ctx.stdout, "removed directory {}", q(disp))?;
                        }
                        Ok(true)
                    }
                    Err(e) => {
                        ctx.err(format!("cannot remove {}: {}", q(disp), msg(&e)));
                        Ok(false)
                    }
                };
            }
            ctx.err(format!("cannot remove {}: Is a directory", q(disp)));
            return Ok(false);
        }
        let mut ok = true;
        match sorted_entries(p) {
            Ok(entries) => {
                for e in entries {
                    let name = e.file_name().to_string_lossy().into_owned();
                    ok &= rm_path(ctx, &e.path(), &join_disp(disp, &name), o)?;
                }
            }
            Err(e) => {
                ctx.err(format!("cannot remove {}: {}", q(disp), msg(&e)));
                return Ok(false);
            }
        }
        if !ok {
            return Ok(false);
        }
        match fs::remove_dir(p) {
            Ok(()) => {
                if o.verbose {
                    writeln!(ctx.stdout, "removed directory {}", q(disp))?;
                }
                Ok(true)
            }
            Err(e) => {
                ctx.err(format!("cannot remove {}: {}", q(disp), msg(&e)));
                Ok(false)
            }
        }
    } else {
        match remove_file_force(p, o.force) {
            Ok(()) => {
                if o.verbose {
                    writeln!(ctx.stdout, "removed {}", q(disp))?;
                }
                Ok(true)
            }
            Err(e) => {
                ctx.err(format!("cannot remove {}: {}", q(disp), msg(&e)));
                Ok(false)
            }
        }
    }
}

pub fn rm(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "rRfvdi", &["recursive", "force", "verbose", "dir"]);
    let ro = RmOpts {
        recursive: o.flag('r') || o.flag('R') || o.long("recursive"),
        force: o.flag('f') || o.long("force"),
        verbose: o.flag('v') || o.long("verbose"),
        dir: o.flag('d') || o.long("dir"),
    };
    if o.operands.is_empty() {
        if ro.force {
            return Ok(0);
        }
        return usage(ctx, "missing operand", 1);
    }
    let mut status = 0;
    for n in &o.operands {
        let b = base_name(n);
        if b == "." || b == ".." {
            ctx.err(format!("refusing to remove '.' or '..' directory: skipping {}", q(n)));
            status = 1;
            continue;
        }
        if !rm_path(ctx, &ctx.path(n), n, &ro)? {
            status = 1;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// cp, mv

#[derive(Clone, Copy)]
struct CpOpts {
    recursive: bool,
    no_clobber: bool,
    force: bool,
    verbose: bool,
    preserve: bool,
}

fn preserve_times(src: &Metadata, dst: &Path) -> io::Result<()> {
    let mut t = fs::FileTimes::new();
    if let Ok(m) = src.modified() {
        t = t.set_modified(m);
    }
    if let Ok(a) = src.accessed() {
        t = t.set_accessed(a);
    }
    open_for_times(dst)?.set_times(t)
}

fn is_inside(child: &Path, parent: &Path) -> bool {
    match (canonicalize_loose(child, false, true), fs::canonicalize(parent)) {
        (Ok(c), Ok(p)) => c.starts_with(&p),
        _ => false,
    }
}

/// Copy `src` to `dst`, reporting errors. `top` marks command-line operands
/// (which are followed when they are symlinks).
fn cp_path(ctx: &mut Ctx, src: &Path, dst: &Path, sdisp: &str, ddisp: &str, o: CpOpts, top: bool) -> io::Result<bool> {
    ctx.check_interrupt()?;
    let lm = match if top && !o.recursive { fs::metadata(src) } else { fs::symlink_metadata(src) } {
        Ok(m) => m,
        Err(e) => {
            ctx.err(format!("cannot stat {}: {}", q(sdisp), msg(&e)));
            return Ok(false);
        }
    };
    if lm.file_type().is_symlink() {
        // recursive copies keep symlinks as symlinks
        let target = fs::read_link(src)?;
        if fs::symlink_metadata(dst).is_ok() {
            if o.no_clobber {
                return Ok(true);
            }
            let _ = fs::remove_file(dst);
        }
        let r = make_symlink(&target, dst);
        if let Err(e) = r {
            ctx.err(format!("cannot create symbolic link {}: {}", q(ddisp), msg(&e)));
            return Ok(false);
        }
        if o.verbose {
            writeln!(ctx.stdout, "{} -> {}", q(sdisp), q(ddisp))?;
        }
        return Ok(true);
    }
    if lm.is_dir() {
        if !o.recursive {
            ctx.err(format!("-r not specified; omitting directory {}", q(sdisp)));
            return Ok(false);
        }
        if top && is_inside(dst, src) {
            ctx.err(format!("cannot copy a directory, {}, into itself, {}", q(sdisp), q(ddisp)));
            return Ok(false);
        }
        match fs::metadata(dst) {
            Ok(m) if !m.is_dir() => {
                ctx.err(format!("cannot overwrite non-directory {} with directory {}", q(ddisp), q(sdisp)));
                return Ok(false);
            }
            Ok(_) => {}
            Err(_) => {
                if let Err(e) = fs::create_dir(dst) {
                    ctx.err(format!("cannot create directory {}: {}", q(ddisp), msg(&e)));
                    return Ok(false);
                }
                if o.verbose {
                    writeln!(ctx.stdout, "{} -> {}", q(sdisp), q(ddisp))?;
                }
            }
        }
        let mut ok = true;
        let entries = match sorted_entries(src) {
            Ok(e) => e,
            Err(e) => {
                ctx.err(format!("cannot access {}: {}", q(sdisp), msg(&e)));
                return Ok(false);
            }
        };
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            ok &= cp_path(ctx, &e.path(), &dst.join(e.file_name()), &join_disp(sdisp, &name), &join_disp(ddisp, &name), o, false)?;
        }
        if o.preserve {
            let _ = fs::set_permissions(dst, lm.permissions());
            let _ = preserve_times(&lm, dst);
        }
        return Ok(ok);
    }
    if let Ok(dm) = fs::metadata(dst) {
        if same_file(src, dst) {
            ctx.err(format!("{} and {} are the same file", q(sdisp), q(ddisp)));
            return Ok(false);
        }
        if o.no_clobber {
            return Ok(true);
        }
        if dm.is_dir() {
            ctx.err(format!("cannot overwrite directory {} with non-directory", q(ddisp)));
            return Ok(false);
        }
    }
    let mut r = fs::copy(src, dst);
    if r.is_err() && o.force && fs::symlink_metadata(dst).is_ok() {
        let _ = remove_file_force(dst, true);
        r = fs::copy(src, dst);
    }
    if let Err(e) = r {
        ctx.err(format!("cannot create regular file {}: {}", q(ddisp), msg(&e)));
        return Ok(false);
    }
    if o.preserve {
        let _ = preserve_times(&lm, dst);
    }
    if o.verbose {
        writeln!(ctx.stdout, "{} -> {}", q(sdisp), q(ddisp))?;
    }
    Ok(true)
}

fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        let base = link.parent().unwrap_or(Path::new("."));
        let resolved = if target.is_absolute() { target.to_path_buf() } else { base.join(target) };
        let r = if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        };
        r.map_err(|e| {
            if e.kind() == ErrorKind::PermissionDenied || e.raw_os_error() == Some(1314) {
                io::Error::new(e.kind(), "Permission denied (creating symlinks on Windows needs Developer Mode or administrator rights)")
            } else {
                e
            }
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(io::Error::new(ErrorKind::Unsupported, "symbolic links are not supported"))
    }
}

/// Work out (source, destination) pairs for cp/mv/ln-style operands.
fn targets(ctx: &mut Ctx, ops: &[String], no_target_dir: bool) -> Option<Vec<(String, PathBuf, String)>> {
    if ops.len() < 2 {
        if ops.is_empty() {
            ctx.err("missing file operand");
        } else {
            ctx.err(format!("missing destination file operand after {}", q(&ops[0])));
        }
        return None;
    }
    let (srcs, dest) = ops.split_at(ops.len() - 1);
    let dest = &dest[0];
    let dpath = ctx.path(dest);
    let into_dir = !no_target_dir && dpath.is_dir();
    if srcs.len() > 1 && !into_dir {
        ctx.err(format!("target {} is not a directory", q(dest)));
        return None;
    }
    Some(
        srcs.iter()
            .map(|s| {
                if into_dir {
                    let b = base_name(s);
                    (s.clone(), dpath.join(&b), join_disp(dest, &b))
                } else {
                    (s.clone(), dpath.clone(), dest.clone())
                }
            })
            .collect(),
    )
}

pub fn cp(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "rRfnvpaT", &["recursive", "force", "no-clobber", "verbose", "preserve", "archive", "no-target-directory"]);
    let co = CpOpts {
        recursive: o.flag('r') || o.flag('R') || o.flag('a') || o.long("recursive") || o.long("archive"),
        no_clobber: o.flag('n') || o.long("no-clobber"),
        force: o.flag('f') || o.long("force"),
        verbose: o.flag('v') || o.long("verbose"),
        preserve: o.flag('p') || o.flag('a') || o.long("preserve") || o.long("archive"),
    };
    let Some(pairs) = targets(ctx, &o.operands, o.flag('T')) else { return Ok(1) };
    let mut status = 0;
    for (s, d, ddisp) in pairs {
        let sp = ctx.path(&s);
        if !cp_path(ctx, &sp, &d, &s, &ddisp, co, true)? {
            status = 1;
        }
    }
    Ok(status)
}

pub fn mv(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "fnviT", &["force", "no-clobber", "verbose", "no-target-directory"]);
    let no_clobber = o.flag('n') || o.long("no-clobber");
    let verbose = o.flag('v') || o.long("verbose");
    let Some(pairs) = targets(ctx, &o.operands, o.flag('T')) else { return Ok(1) };
    let mut status = 0;
    for (s, d, ddisp) in pairs {
        let sp = ctx.path(&s);
        let sm = match fs::symlink_metadata(&sp) {
            Ok(m) => m,
            Err(e) => {
                ctx.err(format!("cannot stat {}: {}", q(&s), msg(&e)));
                status = 1;
                continue;
            }
        };
        if fs::symlink_metadata(&d).is_ok() {
            if same_file(&sp, &d) && fs::symlink_metadata(&d).map(|m| m.file_type().is_symlink()).unwrap_or(false) == sm.file_type().is_symlink() {
                ctx.err(format!("{} and {} are the same file", q(&s), q(&ddisp)));
                status = 1;
                continue;
            }
            if no_clobber {
                continue;
            }
        }
        if sm.is_dir() && is_inside(&d, &sp) {
            ctx.err(format!("cannot move {} to a subdirectory of itself, {}", q(&s), q(&ddisp)));
            status = 1;
            continue;
        }
        match fs::rename(&sp, &d) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::CrossesDevices => {
                let co = CpOpts { recursive: true, no_clobber: false, force: true, verbose: false, preserve: true };
                if !cp_path(ctx, &sp, &d, &s, &ddisp, co, false)? {
                    status = 1;
                    continue;
                }
                let ro = RmOpts { recursive: true, force: true, verbose: false, dir: true };
                if !rm_path(ctx, &sp, &s, &ro)? {
                    status = 1;
                    continue;
                }
            }
            Err(e) => {
                ctx.err(format!("cannot move {} to {}: {}", q(&s), q(&ddisp), msg(&e)));
                status = 1;
                continue;
            }
        }
        if verbose {
            writeln!(ctx.stdout, "renamed {} -> {}", q(&s), q(&ddisp))?;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// ln

pub fn ln(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "sfvnT", &["symbolic", "force", "verbose", "no-dereference", "no-target-directory"]);
    let symbolic = o.flag('s') || o.long("symbolic");
    let force = o.flag('f') || o.long("force");
    let verbose = o.flag('v') || o.long("verbose");
    let pairs: Vec<(String, PathBuf, String)> = if o.operands.len() == 1 {
        let b = base_name(&o.operands[0]);
        vec![(o.operands[0].clone(), ctx.path(&b), b)]
    } else {
        match targets(ctx, &o.operands, o.flag('T')) {
            Some(p) => p,
            None => return Ok(1),
        }
    };
    let mut status = 0;
    for (target, link, ldisp) in pairs {
        if let Ok(m) = fs::symlink_metadata(&link) {
            if force && !m.is_dir() {
                let _ = remove_file_force(&link, true);
            } else if force && m.file_type().is_symlink() {
                let _ = fs::remove_file(&link).or_else(|_| fs::remove_dir(&link));
            }
        }
        let r = if symbolic {
            make_symlink(Path::new(&target), &link)
        } else {
            fs::hard_link(ctx.path(&target), &link)
        };
        match r {
            Ok(()) => {
                if verbose {
                    let arrow = if symbolic { "->" } else { "=>" };
                    writeln!(ctx.stdout, "{} {arrow} {}", q(&ldisp), q(&target))?;
                }
            }
            Err(e) => {
                if symbolic {
                    ctx.err(format!("failed to create symbolic link {}: {}", q(&ldisp), msg(&e)));
                } else {
                    ctx.err(format!("failed to create hard link {} => {}: {}", q(&ldisp), q(&target), msg(&e)));
                }
                status = 1;
            }
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// chmod (unix)

/// Apply an octal or symbolic mode (`u+x,go-w`, `a=r`, `+X`) to `cur`.
#[cfg(unix)]
pub fn parse_mode(spec: &str, cur: u32, is_dir: bool) -> Option<u32> {
    if !spec.is_empty() && spec.chars().all(|c| ('0'..='7').contains(&c)) {
        let v = u32::from_str_radix(spec, 8).ok()?;
        if v > 0o7777 {
            return None;
        }
        return Some(v);
    }
    let mut mode = cur & 0o7777;
    for clause in spec.split(',') {
        let b: Vec<char> = clause.chars().collect();
        let mut i = 0;
        let mut who = 0u32;
        while i < b.len() && "ugoa".contains(b[i]) {
            who |= match b[i] {
                'u' => 0o4700,
                'g' => 0o2070,
                'o' => 0o1007,
                _ => 0o7777,
            };
            i += 1;
        }
        if who == 0 {
            who = 0o7777;
        }
        if i >= b.len() {
            return None;
        }
        while i < b.len() {
            let op = b[i];
            if !"+-=".contains(op) {
                return None;
            }
            i += 1;
            let mut perm = 0u32;
            while i < b.len() && !"+-=".contains(b[i]) {
                perm |= match b[i] {
                    'r' => 0o444,
                    'w' => 0o222,
                    'x' => 0o111,
                    'X' => {
                        if is_dir || mode & 0o111 != 0 {
                            0o111
                        } else {
                            0
                        }
                    }
                    's' => 0o6000,
                    't' => 0o1000,
                    'u' => {
                        let u = (mode >> 6) & 7;
                        u * 0o111
                    }
                    'g' => {
                        let g = (mode >> 3) & 7;
                        g * 0o111
                    }
                    'o' => {
                        let o = mode & 7;
                        o * 0o111
                    }
                    _ => return None,
                };
                i += 1;
            }
            let bits = perm & who;
            match op {
                '+' => mode |= bits,
                '-' => mode &= !bits,
                _ => mode = (mode & !(who & 0o7777)) | bits,
            }
        }
    }
    Some(mode)
}

#[cfg(unix)]
pub fn chmod(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    use std::os::unix::fs::PermissionsExt;
    let mut recursive = false;
    let mut verbose = false;
    let mut changes = false;
    let mut quiet = false;
    let mut mode: Option<String> = None;
    let mut files = Vec::new();
    let mut only_ops = false;
    for a in args {
        if only_ops {
            files.push(a.clone());
            continue;
        }
        match a.as_str() {
            "--" => only_ops = true,
            "-R" | "--recursive" => recursive = true,
            "-v" | "--verbose" => verbose = true,
            "-c" | "--changes" => changes = true,
            "-f" | "--silent" | "--quiet" => quiet = true,
            _ if a.starts_with("--") => return usage(ctx, format!("unrecognized option {}", q(a)), 1),
            _ if mode.is_none() => mode = Some(a.clone()),
            _ => files.push(a.clone()),
        }
    }
    let Some(mode) = mode else { return usage(ctx, "missing operand", 1) };
    if files.is_empty() {
        return usage(ctx, format!("missing operand after {}", q(&mode)), 1);
    }
    if parse_mode(&mode, 0o644, false).is_none() {
        return usage(ctx, format!("invalid mode: {}", q(&mode)), 1);
    }
    #[allow(clippy::too_many_arguments)]
    fn apply(ctx: &mut Ctx, p: &Path, disp: &str, mode: &str, rec: bool, verbose: bool, changes: bool, quiet: bool) -> io::Result<bool> {
        ctx.check_interrupt()?;
        let m = match fs::metadata(p) {
            Ok(m) => m,
            Err(e) => {
                if !quiet {
                    ctx.err(format!("cannot access {}: {}", q(disp), msg(&e)));
                }
                return Ok(false);
            }
        };
        let old = m.permissions().mode() & 0o7777;
        let new = parse_mode(mode, old, m.is_dir()).unwrap_or(old);
        let mut ok = true;
        if new != old
            && let Err(e) = fs::set_permissions(p, fs::Permissions::from_mode(new)) {
                if !quiet {
                    ctx.err(format!("changing permissions of {}: {}", q(disp), msg(&e)));
                }
                ok = false;
            }
        if ok && (verbose || (changes && new != old)) {
            if new != old {
                writeln!(ctx.stdout, "mode of {} changed from {:04o} ({}) to {:04o} ({})", q(disp), old, perm_string(old), new, perm_string(new))?;
            } else {
                writeln!(ctx.stdout, "mode of {} retained as {:04o} ({})", q(disp), old, perm_string(old))?;
            }
        }
        if rec && m.is_dir()
            && let Ok(entries) = sorted_entries(p) {
                for e in entries {
                    if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                        continue;
                    }
                    let name = e.file_name().to_string_lossy().into_owned();
                    ok &= apply(ctx, &e.path(), &join_disp(disp, &name), mode, rec, verbose, changes, quiet)?;
                }
            }
        Ok(ok)
    }
    let mut status = 0;
    for f in &files {
        let p = ctx.path(f);
        if !apply(ctx, &p, f, &mode, recursive, verbose, changes, quiet)? {
            status = 1;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// mktemp

fn temp_dir(ctx: &Ctx) -> PathBuf {
    for k in ["TMPDIR", "TMP", "TEMP"] {
        if let Some(v) = ctx.getenv(k).filter(|v| !v.is_empty()) {
            return PathBuf::from(v);
        }
    }
    if cfg!(windows) {
        match ctx.getenv("SystemRoot") {
            Some(r) => Path::new(r).join("Temp"),
            None => PathBuf::from("C:/Windows/Temp"),
        }
    } else {
        PathBuf::from("/tmp")
    }
}

pub fn mktemp(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "p", long_with_value: &["suffix"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "dpqtu", &["directory", "tmpdir", "quiet", "dry-run", "suffix"]);
    let dir_mode = o.flag('d') || o.long("directory");
    let quiet = o.flag('q') || o.long("quiet");
    if o.operands.len() > 1 {
        return usage(ctx, "too many templates", 1);
    }
    let given = o.operands.first().cloned();
    let template = given.clone().unwrap_or_else(|| "tmp.XXXXXXXXXX".into());
    let suffix = o.long_value("suffix").unwrap_or("").to_string();
    let xs = template.chars().rev().take_while(|&c| c == 'X').count();
    if xs < 3 {
        if !quiet {
            ctx.err(format!("too few X's in template {}", q(&template)));
        }
        return Ok(1);
    }
    let tmpdir_opt = o.value('p').map(String::from).or_else(|| o.long_value("tmpdir").map(String::from));
    let base: PathBuf = if let Some(d) = tmpdir_opt.filter(|d| !d.is_empty()) {
        ctx.path(&d)
    } else if o.long("tmpdir") || o.flag('p') || o.flag('t') || given.is_none() {
        temp_dir(ctx)
    } else {
        ctx.cwd.to_path_buf()
    };
    let prefix = &template[..template.len() - xs];
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    for _ in 0..100 {
        let mut name = prefix.to_string();
        let mut r = random_u64();
        for k in 0..xs {
            if k % 10 == 9 {
                r = random_u64();
            }
            name.push(ALNUM[(r % ALNUM.len() as u64) as usize] as char);
            r /= ALNUM.len() as u64;
        }
        name.push_str(&suffix);
        let path = base.join(&name);
        let res = if o.flag('u') || o.long("dry-run") {
            if path.exists() { Err(io::Error::from(ErrorKind::AlreadyExists)) } else { Ok(()) }
        } else if dir_mode {
            #[cfg_attr(not(unix), allow(unused_mut))]
            let mut b = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(&path)
        } else {
            let mut op = fs::OpenOptions::new();
            op.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                op.mode(0o600);
            }
            op.open(&path).map(|_| ())
        };
        match res {
            Ok(()) => {
                writeln!(ctx.stdout, "{}", display(&path))?;
                return Ok(0);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => {
                if !quiet {
                    let what = if dir_mode { "directory" } else { "file" };
                    ctx.err(format!("failed to create {what} via template {}: {}", q(&display(&base.join(&template))), msg(&e)));
                }
                return Ok(1);
            }
        }
    }
    if !quiet {
        ctx.err(format!("failed to create file via template {}: File exists", q(&template)));
    }
    Ok(1)
}

// ---------------------------------------------------------------------------
// stat

struct StatInfo {
    size: u64,
    blocks: u64,
    blksize: u64,
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    uid: u32,
    gid: u32,
}

fn stat_info(m: &Metadata) -> StatInfo {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        StatInfo {
            size: m.len(),
            blocks: m.blocks(),
            blksize: m.blksize(),
            dev: m.dev(),
            ino: m.ino(),
            nlink: m.nlink(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
        }
    }
    #[cfg(not(unix))]
    {
        let ft = m.file_type();
        let mut mode = if ft.is_dir() { 0o040755 } else if ft.is_symlink() { 0o120777 } else { 0o100644 };
        if m.permissions().readonly() {
            mode &= !0o222;
        }
        StatInfo { size: m.len(), blocks: m.len().div_ceil(512), blksize: 4096, dev: 0, ino: 0, nlink: 1, mode, uid: 0, gid: 0 }
    }
}

fn owner_names(si: &StatInfo) -> (String, String) {
    #[cfg(unix)]
    {
        (user_name(si.uid).unwrap_or_else(|| si.uid.to_string()), group_name(si.gid).unwrap_or_else(|| si.gid.to_string()))
    }
    #[cfg(not(unix))]
    {
        let _ = si;
        ("-".into(), "-".into())
    }
}

fn ctime(m: &Metadata) -> Option<SystemTime> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let secs = m.ctime();
        let nsec = m.ctime_nsec();
        if secs >= 0 {
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(secs as u64, nsec as u32))
        } else {
            None
        }
    }
    #[cfg(not(unix))]
    {
        m.modified().ok()
    }
}

fn stat_format(ctx: &Ctx, fmt: &str, name: &str, path: &Path, m: &Metadata) -> String {
    let si = stat_info(m);
    let tz = local_tz(ctx);
    let human_time = |t: Option<SystemTime>| -> String {
        match t.and_then(|t| to_zoned(t, &tz)) {
            Some(z) => strftime(&z, "%Y-%m-%d %H:%M:%S.%N %z"),
            None => "-".into(),
        }
    };
    let epoch = |t: Option<SystemTime>| -> String {
        match t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()) {
            Some(d) => d.as_secs().to_string(),
            None => "0".into(),
        }
    };
    let quoted_name = || -> String {
        if m.file_type().is_symlink() {
            match fs::read_link(path) {
                Ok(t) => format!("{} -> {}", q(name), q(&t.to_string_lossy())),
                Err(_) => q(name),
            }
        } else {
            q(name)
        }
    };
    let mut out = String::new();
    let mut it = fmt.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(d) = it.next() else {
            out.push('%');
            break;
        };
        let (u, g) = if matches!(d, 'U' | 'G') { owner_names(&si) } else { (String::new(), String::new()) };
        match d {
            '%' => out.push('%'),
            'n' => out.push_str(name),
            'N' => out.push_str(&quoted_name()),
            's' => out.push_str(&si.size.to_string()),
            'b' => out.push_str(&si.blocks.to_string()),
            'B' => out.push_str("512"),
            'o' => out.push_str(&si.blksize.to_string()),
            'f' => out.push_str(&format!("{:x}", si.mode)),
            'a' => out.push_str(&format!("{:o}", si.mode & 0o7777)),
            'A' => out.push_str(&mode_string(m)),
            'F' => out.push_str(file_type_name(m)),
            'u' => out.push_str(&si.uid.to_string()),
            'g' => out.push_str(&si.gid.to_string()),
            'U' => out.push_str(&u),
            'G' => out.push_str(&g),
            'h' => out.push_str(&si.nlink.to_string()),
            'i' => out.push_str(&si.ino.to_string()),
            'd' => out.push_str(&si.dev.to_string()),
            'D' => out.push_str(&format!("{:x}", si.dev)),
            'x' => out.push_str(&human_time(m.accessed().ok())),
            'y' => out.push_str(&human_time(m.modified().ok())),
            'z' => out.push_str(&human_time(ctime(m))),
            'w' => out.push_str(&human_time(m.created().ok())),
            'X' => out.push_str(&epoch(m.accessed().ok())),
            'Y' => out.push_str(&epoch(m.modified().ok())),
            'Z' => out.push_str(&epoch(ctime(m))),
            'W' => out.push_str(&epoch(m.created().ok())),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

pub fn stat(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "c", long_with_value: &["format", "printf"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "cLt", &["format", "printf", "dereference", "terse"]);
    if o.operands.is_empty() {
        return usage(ctx, "missing operand", 1);
    }
    let follow = o.flag('L') || o.long("dereference");
    let user_fmt = o.value('c').or(o.long_value("format")).map(|f| format!("{f}\n")).or_else(|| {
        o.long_value("printf").map(|f| String::from_utf8_lossy(&super::common::unescape(f, true).0).into_owned())
    });
    let mut status = 0;
    for n in &o.operands {
        let p = ctx.path(n);
        let m = if follow { fs::metadata(&p) } else { fs::symlink_metadata(&p) };
        let m = match m {
            Ok(m) => m,
            Err(e) => {
                ctx.err(format!("cannot stat {}: {}", q(n), msg(&e)));
                status = 1;
                continue;
            }
        };
        let fmt = match &user_fmt {
            Some(f) => f.clone(),
            None if o.flag('t') || o.long("terse") => "%n %s %b %f %u %g %D %i %h %X %Y %Z %W %o\n".to_string(),
            None => "  File: %N\n  Size: %s\tBlocks: %b\tIO Block: %o\t%F\nDevice: %d\tInode: %i\tLinks: %h\nAccess: (0%a/%A)  Uid: (%u/%U)   Gid: (%g/%G)\nAccess: %x\nModify: %y\nChange: %z\n Birth: %w\n"
                .to_string(),
        };
        let s = stat_format(ctx, &fmt, n, &p, &m);
        ctx.stdout.write_all(s.as_bytes())?;
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// du

struct DuOpts {
    all: bool,
    summarize: bool,
    human: bool,
    bytes: bool,
    max_depth: Option<usize>,
    #[cfg(unix)]
    seen: std::collections::HashSet<(u64, u64)>,
}

fn disk_usage(m: &Metadata, apparent: bool) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !apparent {
            return m.blocks() * 512;
        }
    }
    let _ = apparent;
    m.len()
}

fn du_print(ctx: &mut Ctx, o: &DuOpts, size: u64, disp: &str) -> io::Result<()> {
    let s = if o.human {
        human(size)
    } else if o.bytes {
        size.to_string()
    } else {
        size.div_ceil(1024).to_string()
    };
    writeln!(ctx.stdout, "{s}\t{disp}")
}

fn du_walk(ctx: &mut Ctx, p: &Path, disp: &str, depth: usize, o: &mut DuOpts, failed: &mut bool) -> io::Result<u64> {
    ctx.check_interrupt()?;
    let m = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) => {
            ctx.err(format!("cannot access {}: {}", q(disp), msg(&e)));
            *failed = true;
            return Ok(0);
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !m.is_dir() && m.nlink() > 1 && !o.seen.insert((m.dev(), m.ino())) {
            return Ok(0);
        }
    }
    let mut total = disk_usage(&m, o.bytes);
    let within = |d: usize, o: &DuOpts| o.max_depth.is_none_or(|max| d <= max);
    if m.is_dir() {
        match sorted_entries(p) {
            Ok(entries) => {
                for e in entries {
                    let name = e.file_name().to_string_lossy().into_owned();
                    total += du_walk(ctx, &e.path(), &join_disp(disp, &name), depth + 1, o, failed)?;
                }
            }
            Err(e) => {
                ctx.err(format!("cannot read directory {}: {}", q(disp), msg(&e)));
                *failed = true;
            }
        }
        if (!o.summarize || depth == 0) && within(depth, o) {
            du_print(ctx, o, total, disp)?;
        }
    } else if depth == 0 || (o.all && !o.summarize && within(depth, o)) {
        du_print(ctx, o, total, disp)?;
    }
    Ok(total)
}

pub fn du(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "d", long_with_value: &["max-depth"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "ashbckd", &["all", "summarize", "human-readable", "bytes", "total", "max-depth", "apparent-size"]);
    let max_depth = match o.value('d').or(o.long_value("max-depth")) {
        Some(v) => match v.parse::<usize>() {
            Ok(n) => Some(n),
            Err(_) => return usage(ctx, format!("invalid maximum depth {}", q(v)), 1),
        },
        None => None,
    };
    let mut d = DuOpts {
        all: o.flag('a') || o.long("all"),
        summarize: o.flag('s') || o.long("summarize"),
        human: o.flag('h') || o.long("human-readable"),
        bytes: o.flag('b') || o.long("bytes") || o.long("apparent-size"),
        max_depth,
        #[cfg(unix)]
        seen: Default::default(),
    };
    if d.summarize && d.all {
        return usage(ctx, "cannot both summarize and show all entries", 1);
    }
    let ops = if o.operands.is_empty() { vec![".".to_string()] } else { o.operands.clone() };
    let mut failed = false;
    let mut grand = 0;
    for n in &ops {
        let p = ctx.path(n);
        grand += du_walk(ctx, &p, n, 0, &mut d, &mut failed)?;
    }
    if o.flag('c') || o.long("total") {
        du_print(ctx, &d, grand, "total")?;
    }
    Ok(if failed { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::common::testutil::TempDir;
    use crate::utils::run_capture;

    #[test]
    fn names() {
        let r = |n: &str, a: &[&str]| run_capture(n, a, b"", Path::new(".")).1;
        assert_eq!(r("basename", &["/a/b/c.txt"]), "c.txt\n");
        assert_eq!(r("basename", &["/a/b/c.txt", ".txt"]), "c\n");
        assert_eq!(r("basename", &["-s", ".rs", "a.rs", "b/c.rs"]), "a\nc\n");
        assert_eq!(r("basename", &["-a", "x/y", "z/"]), "y\nz\n");
        assert_eq!(r("basename", &["/"]), "/\n");
        assert_eq!(r("dirname", &["/a/b/c"]), "/a/b\n");
        assert_eq!(r("dirname", &["a"]), ".\n");
        assert_eq!(r("dirname", &["/a"]), "/\n");
        assert_eq!(r("dirname", &["a/b/", "x//y"]), "a\nx\n");
        assert_eq!(run_capture("basename", &[], b"", Path::new(".")).0, 1);
    }

    #[test]
    fn mkdir_rmdir() {
        let d = TempDir::new("mkdir");
        let (s, out, _) = run_capture("mkdir", &["-pv", "a/b/c"], b"", d.path());
        assert_eq!(s, 0);
        assert_eq!(out, "mkdir: created directory 'a'\nmkdir: created directory 'a/b'\nmkdir: created directory 'a/b/c'\n");
        assert!(d.path().join("a/b/c").is_dir());
        let (s, _, e) = run_capture("mkdir", &["a"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "mkdir: cannot create directory 'a': File exists\n"));
        assert_eq!(run_capture("mkdir", &["-p", "a"], b"", d.path()).0, 0);
        let (s, _, e) = run_capture("rmdir", &["a"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "rmdir: failed to remove 'a': Directory not empty\n"));
        assert_eq!(run_capture("rmdir", &["-p", "a/b/c"], b"", d.path()).0, 0);
        assert!(!d.exists("a"));
    }

    #[cfg(unix)]
    #[test]
    fn mkdir_mode() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new("mkdirm");
        assert_eq!(run_capture("mkdir", &["-m", "700", "x"], b"", d.path()).0, 0);
        assert_eq!(fs::metadata(d.path().join("x")).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn touch_and_rm() {
        let d = TempDir::new("touch");
        assert_eq!(run_capture("touch", &["f1", "f2"], b"", d.path()).0, 0);
        assert!(d.exists("f1") && d.exists("f2"));
        assert_eq!(run_capture("touch", &["-c", "nope"], b"", d.path()).0, 0);
        assert!(!d.exists("nope"));
        assert_eq!(run_capture("touch", &["-d", "2020-01-02 03:04:05", "f1"], b"", d.path()).0, 0);
        let m = fs::metadata(d.path().join("f1")).unwrap().modified().unwrap();
        let ts = jiff::Timestamp::try_from(m).unwrap();
        assert_eq!(ts.to_zoned(jiff::tz::TimeZone::system()).year(), 2020);
        d.write("keep", "data");
        assert_eq!(run_capture("touch", &["keep"], b"", d.path()).0, 0);
        assert_eq!(d.read("keep"), "data");

        let (s, out, _) = run_capture("rm", &["-v", "f1"], b"", d.path());
        assert_eq!((s, out.as_str()), (0, "removed 'f1'\n"));
        let (s, _, e) = run_capture("rm", &["f1"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "rm: cannot remove 'f1': No such file or directory\n"));
        assert_eq!(run_capture("rm", &["-f", "f1"], b"", d.path()).0, 0);
        d.write("dir/sub/x", "1");
        let (s, _, e) = run_capture("rm", &["dir"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "rm: cannot remove 'dir': Is a directory\n"));
        let (s, out, _) = run_capture("rm", &["-rv", "dir"], b"", d.path());
        assert_eq!(s, 0);
        assert_eq!(out, "removed 'dir/sub/x'\nremoved directory 'dir/sub'\nremoved directory 'dir'\n");
        fs::create_dir(d.path().join("empty")).unwrap();
        assert_eq!(run_capture("rm", &["-d", "empty"], b"", d.path()).0, 0);
        assert_eq!(run_capture("rm", &["-r", "."], b"", d.path()).0, 1);
        assert_eq!(run_capture("rm", &[], b"", d.path()).0, 1);
    }

    #[test]
    fn cp_mv() {
        let d = TempDir::new("cp");
        d.write("a", "hello");
        let (s, out, _) = run_capture("cp", &["-v", "a", "b"], b"", d.path());
        assert_eq!((s, out.as_str()), (0, "'a' -> 'b'\n"));
        assert_eq!(d.read("b"), "hello");
        fs::create_dir(d.path().join("dir")).unwrap();
        assert_eq!(run_capture("cp", &["a", "b", "dir"], b"", d.path()).0, 0);
        assert_eq!(d.read("dir/a"), "hello");
        assert_eq!(d.read("dir/b"), "hello");
        let (s, _, e) = run_capture("cp", &["a", "b", "zz"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "cp: target 'zz' is not a directory\n"));
        let (s, _, e) = run_capture("cp", &["dir", "dir2"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "cp: -r not specified; omitting directory 'dir'\n"));
        assert_eq!(run_capture("cp", &["-r", "dir", "dir2"], b"", d.path()).0, 0);
        assert_eq!(d.read("dir2/a"), "hello");
        let (s, _, e) = run_capture("cp", &["-r", "dir", "dir/inner"], b"", d.path());
        assert_eq!(s, 1);
        assert!(e.contains("into itself"), "{e}");
        d.write("c", "other");
        assert_eq!(run_capture("cp", &["-n", "c", "a"], b"", d.path()).0, 0);
        assert_eq!(d.read("a"), "hello");
        let (s, _, e) = run_capture("cp", &["a", "a"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "cp: 'a' and 'a' are the same file\n"));
        assert_eq!(run_capture("cp", &["-p", "a", "p"], b"", d.path()).0, 0);
        let ma = fs::metadata(d.path().join("a")).unwrap().modified().unwrap();
        let mp = fs::metadata(d.path().join("p")).unwrap().modified().unwrap();
        assert_eq!(ma, mp);

        let (s, out, _) = run_capture("mv", &["-v", "c", "moved"], b"", d.path());
        assert_eq!((s, out.as_str()), (0, "renamed 'c' -> 'moved'\n"));
        assert!(!d.exists("c"));
        assert_eq!(d.read("moved"), "other");
        assert_eq!(run_capture("mv", &["moved", "dir"], b"", d.path()).0, 0);
        assert_eq!(d.read("dir/moved"), "other");
        assert_eq!(run_capture("mv", &["-n", "a", "b"], b"", d.path()).0, 0);
        assert!(d.exists("a"));
        let (s, _, e) = run_capture("mv", &["dir", "dir/sub"], b"", d.path());
        assert_eq!(s, 1);
        assert!(e.contains("subdirectory of itself"), "{e}");
        let (s, _, e) = run_capture("mv", &["nope", "x"], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "mv: cannot stat 'nope': No such file or directory\n"));
    }

    #[cfg(unix)]
    #[test]
    fn links_and_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new("ln");
        d.write("t", "x");
        assert_eq!(run_capture("ln", &["-s", "t", "l"], b"", d.path()).0, 0);
        assert_eq!(fs::read_link(d.path().join("l")).unwrap(), Path::new("t"));
        let (s, _, e) = run_capture("ln", &["-s", "t", "l"], b"", d.path());
        assert_eq!(s, 1);
        assert!(e.contains("File exists"));
        assert_eq!(run_capture("ln", &["-sf", "t2", "l"], b"", d.path()).0, 0);
        assert_eq!(fs::read_link(d.path().join("l")).unwrap(), Path::new("t2"));
        assert_eq!(run_capture("ln", &["t", "h"], b"", d.path()).0, 0);
        assert_eq!(d.read("h"), "x");
        let (_, out, _) = run_capture("readlink", &["l"], b"", d.path());
        assert_eq!(out, "t2\n");
        assert_eq!(run_capture("readlink", &["t"], b"", d.path()).0, 1);
        let (_, out, _) = run_capture("readlink", &["-f", "h"], b"", d.path());
        assert_eq!(out, format!("{}\n", fs::canonicalize(d.path()).unwrap().join("h").display()));
        let (_, out, _) = run_capture("realpath", &["-m", "no/such"], b"", d.path());
        assert_eq!(out, format!("{}\n", fs::canonicalize(d.path()).unwrap().join("no/such").display()));

        fs::set_permissions(d.path().join("t"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(run_capture("chmod", &["u+x,go-r", "t"], b"", d.path()).0, 0);
        assert_eq!(fs::metadata(d.path().join("t")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(run_capture("chmod", &["644", "t"], b"", d.path()).0, 0);
        assert_eq!(fs::metadata(d.path().join("t")).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(run_capture("chmod", &["-w", "t"], b"", d.path()).0, 0);
        assert_eq!(fs::metadata(d.path().join("t")).unwrap().permissions().mode() & 0o777, 0o444);
        let (_, out, _) = run_capture("chmod", &["-v", "a=rw", "t"], b"", d.path());
        assert_eq!(out, "mode of 't' changed from 0444 (r--r--r--) to 0666 (rw-rw-rw-)\n");
        assert_eq!(run_capture("chmod", &["bogus", "t"], b"", d.path()).0, 1);
        assert_eq!(parse_mode("+X", 0o644, true), Some(0o755));
        assert_eq!(parse_mode("+X", 0o644, false), Some(0o644));
        assert_eq!(parse_mode("g=u", 0o740, false), Some(0o770));
    }

    #[test]
    fn mktemp_works() {
        let d = TempDir::new("mktemp");
        let (s, out, _) = run_capture("mktemp", &["-p", d.path().to_str().unwrap(), "fooXXXX"], b"", d.path());
        assert_eq!(s, 0);
        let p = PathBuf::from(out.trim());
        assert!(p.is_file());
        assert!(p.file_name().unwrap().to_str().unwrap().starts_with("foo"));
        let (s, out, _) = run_capture("mktemp", &["-d", "dXXXXXX"], b"", d.path());
        assert_eq!(s, 0);
        assert!(PathBuf::from(out.trim()).is_dir());
        assert!(PathBuf::from(out.trim()).file_name().unwrap().to_str().unwrap().starts_with("d"));
        assert_eq!(run_capture("mktemp", &["fooXX"], b"", d.path()).0, 1);
    }

    #[test]
    fn stat_and_du() {
        let d = TempDir::new("stat");
        d.write("f", "12345");
        let (s, out, _) = run_capture("stat", &["-c", "%n %s %F", "f"], b"", d.path());
        assert_eq!((s, out.as_str()), (0, "f 5 regular file\n"));
        let (_, out, _) = run_capture("stat", &["f"], b"", d.path());
        assert!(out.starts_with("  File: 'f'\n  Size: 5"), "{out}");
        assert_eq!(run_capture("stat", &["nope"], b"", d.path()).0, 1);
        d.write("tree/a", &"x".repeat(3000));
        d.write("tree/sub/b", &"y".repeat(5000));
        let (s, out, _) = run_capture("du", &["-b", "-a", "tree"], b"", d.path());
        assert_eq!(s, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.contains(&"3000\ttree/a"), "{out}");
        assert!(lines.contains(&"5000\ttree/sub/b"), "{out}");
        assert!(lines.last().unwrap().ends_with("\ttree"));
        let (_, out, _) = run_capture("du", &["-s", "tree"], b"", d.path());
        assert_eq!(out.lines().count(), 1);
        let (_, out, _) = run_capture("du", &["-d", "0", "-c", "tree"], b"", d.path());
        assert_eq!(out.lines().count(), 2);
        assert!(out.ends_with("\ttotal\n"));
        let (_, out, _) = run_capture("du", &["-sh", "tree"], b"", d.path());
        assert!(out.ends_with("\ttree\n"));
    }
}
