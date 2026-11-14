//! Text-processing utilities.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::time::Duration;

use super::Ctx;
use super::args::{self, Spec};
use super::common::{char_class, msg, parse_count, q, read_line, read_lines, usage, with_input};

fn inputs(ops: &[String]) -> Vec<String> {
    if ops.is_empty() { vec!["-".into()] } else { ops.to_vec() }
}

macro_rules! opts {
    ($ctx:expr, $args:expr, $spec:expr, $shorts:expr, $longs:expr) => {
        match args::parse($args, $spec) {
            Ok(o) => match o.unknown($shorts, $longs) {
                Some(e) => return usage($ctx, e, 1),
                None => o,
            },
            Err(e) => return usage($ctx, e, 1),
        }
    };
}

// ---------------------------------------------------------------------------
// cat

struct CatOpts {
    number: bool,
    nonblank: bool,
    squeeze: bool,
    ends: bool,
    tabs: bool,
    nonprint: bool,
}

fn show_nonprint(b: u8, tabs: bool, out: &mut Vec<u8>) {
    match b {
        b'\t' if !tabs => out.push(b'\t'),
        b'\n' => out.push(b'\n'),
        0..=31 => {
            out.push(b'^');
            out.push(b + 64);
        }
        127 => out.extend_from_slice(b"^?"),
        128..=255 => {
            out.extend_from_slice(b"M-");
            show_nonprint(b - 128, true, out);
        }
        _ => out.push(b),
    }
}

pub fn cat(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "nbsEATvetu", &["number", "number-nonblank", "squeeze-blank", "show-ends", "show-all", "show-tabs", "show-nonprinting"]);
    let co = CatOpts {
        number: o.flag('n') || o.long("number"),
        nonblank: o.flag('b') || o.long("number-nonblank"),
        squeeze: o.flag('s') || o.long("squeeze-blank"),
        ends: o.flag('E') || o.flag('A') || o.flag('e') || o.long("show-ends") || o.long("show-all"),
        tabs: o.flag('T') || o.flag('A') || o.flag('t') || o.long("show-tabs") || o.long("show-all"),
        nonprint: o.flag('v') || o.flag('A') || o.flag('e') || o.flag('t') || o.long("show-nonprinting") || o.long("show-all"),
    };
    let plain = !(co.number || co.nonblank || co.squeeze || co.ends || co.tabs || co.nonprint);
    let mut line_no = 0u64;
    let mut prev_blank = false;
    let mut status = 0;
    for f in inputs(&o.operands) {
        let ok = with_input(ctx, &f, |ctx, r| {
            if plain {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    ctx.check_interrupt()?;
                    let n = match r.read(&mut buf) {
                        Ok(0) => return Ok(()),
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    };
                    ctx.stdout.write_all(&buf[..n])?;
                    if f == "-" {
                        ctx.stdout.flush()?;
                    }
                }
            }
            let mut line = Vec::new();
            let mut out = Vec::new();
            while read_line(r, &mut line)? {
                ctx.check_interrupt()?;
                let has_nl = line.last() == Some(&b'\n');
                let body = if has_nl { &line[..line.len() - 1] } else { &line[..] };
                let blank = body.is_empty();
                if co.squeeze && blank && prev_blank {
                    continue;
                }
                prev_blank = blank;
                out.clear();
                if co.nonblank && !blank || co.number && !co.nonblank {
                    line_no += 1;
                    out.extend_from_slice(format!("{line_no:>6}\t").as_bytes());
                }
                for &b in body {
                    if co.nonprint && b != b'\t' {
                        show_nonprint(b, false, &mut out);
                    } else if co.tabs && b == b'\t' {
                        out.extend_from_slice(b"^I");
                    } else {
                        out.push(b);
                    }
                }
                if has_nl {
                    if co.ends {
                        out.push(b'$');
                    }
                    out.push(b'\n');
                }
                ctx.stdout.write_all(&out)?;
            }
            Ok(())
        })?;
        if !ok {
            status = 1;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// head, tail

/// A `-n`/`-c` count: plain, `-N` (all but last N) or `+N` (from N).
#[derive(Clone, Copy, Debug)]
enum Count {
    First(u64),
    AllBut(u64),
    From(u64),
}

fn parse_head_count(s: &str, tail: bool) -> Option<Count> {
    if let Some(r) = s.strip_prefix('-') {
        return parse_count(r).map(if tail { Count::First } else { Count::AllBut });
    }
    if let Some(r) = s.strip_prefix('+') {
        return parse_count(r).map(if tail { Count::From } else { Count::First });
    }
    parse_count(s).map(Count::First)
}

pub fn head(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "nc", long_with_value: &["lines", "bytes"], allow_number: true, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "ncqvz", &["lines", "bytes", "quiet", "silent", "verbose"]);
    let (bytes, raw) = if let Some(c) = o.value('c').or(o.long_value("bytes")) {
        (true, c.to_string())
    } else {
        (false, o.value('n').or(o.long_value("lines")).map(String::from).or(o.number.clone()).unwrap_or("10".into()))
    };
    let Some(count) = parse_head_count(&raw, false) else {
        return usage(ctx, format!("invalid number of {}: {}", if bytes { "bytes" } else { "lines" }, q(&raw)), 1);
    };
    let files = inputs(&o.operands);
    let headers = (files.len() > 1 || o.flag('v') || o.long("verbose")) && !(o.flag('q') || o.long("quiet") || o.long("silent"));
    let mut status = 0;
    for (i, f) in files.iter().enumerate() {
        if headers {
            let name = if f == "-" { "standard input" } else { f };
            writeln!(ctx.stdout, "{}==> {name} <==", if i > 0 { "\n" } else { "" })?;
        }
        let ok = with_input(ctx, f, |ctx, r| {
            match (bytes, count) {
                (true, Count::First(n)) => {
                    io::copy(&mut r.take(n), ctx.stdout)?;
                }
                (true, Count::AllBut(n)) => {
                    let mut all = Vec::new();
                    r.read_to_end(&mut all)?;
                    let keep = all.len().saturating_sub(n as usize);
                    ctx.stdout.write_all(&all[..keep])?;
                }
                (false, Count::First(n)) => {
                    let mut line = Vec::new();
                    for _ in 0..n {
                        if !read_line(r, &mut line)? {
                            break;
                        }
                        ctx.stdout.write_all(&line)?;
                    }
                }
                (false, Count::AllBut(n)) => {
                    let mut ring: VecDeque<Vec<u8>> = VecDeque::new();
                    let mut line = Vec::new();
                    while read_line(r, &mut line)? {
                        ctx.check_interrupt()?;
                        ring.push_back(std::mem::take(&mut line));
                        if ring.len() as u64 > n {
                            ctx.stdout.write_all(&ring.pop_front().unwrap())?;
                        }
                    }
                }
                (_, Count::From(_)) => unreachable!(),
            }
            Ok(())
        })?;
        if !ok {
            status = 1;
        }
    }
    Ok(status)
}

fn tail_stream(ctx: &mut Ctx, r: &mut dyn BufRead, bytes: bool, count: Count) -> io::Result<()> {
    match (bytes, count) {
        (true, Count::First(n)) => {
            let mut all = Vec::new();
            r.read_to_end(&mut all)?;
            let start = all.len().saturating_sub(n as usize);
            ctx.stdout.write_all(&all[start..])?;
        }
        (true, Count::From(n)) => {
            let skip = n.saturating_sub(1);
            io::copy(&mut r.take(skip), &mut io::sink())?;
            io::copy(r, ctx.stdout)?;
        }
        (false, Count::First(n)) => {
            let mut ring: VecDeque<Vec<u8>> = VecDeque::new();
            let mut line = Vec::new();
            while read_line(r, &mut line)? {
                ctx.check_interrupt()?;
                ring.push_back(std::mem::take(&mut line));
                if ring.len() as u64 > n {
                    ring.pop_front();
                }
            }
            for l in ring {
                ctx.stdout.write_all(&l)?;
            }
        }
        (false, Count::From(n)) => {
            let mut line = Vec::new();
            let mut k = 1;
            while read_line(r, &mut line)? {
                ctx.check_interrupt()?;
                if k >= n {
                    ctx.stdout.write_all(&line)?;
                }
                k += 1;
            }
        }
        (_, Count::AllBut(_)) => unreachable!(),
    }
    Ok(())
}

pub fn tail(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "ncs", long_with_value: &["lines", "bytes", "sleep-interval"], allow_number: true, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "ncqvfFs", &["lines", "bytes", "quiet", "silent", "verbose", "follow", "sleep-interval", "retry"]);
    let (bytes, raw) = if let Some(c) = o.value('c').or(o.long_value("bytes")) {
        (true, c.to_string())
    } else {
        (false, o.value('n').or(o.long_value("lines")).map(String::from).or(o.number.clone()).unwrap_or("10".into()))
    };
    let Some(count) = parse_head_count(&raw, true) else {
        return usage(ctx, format!("invalid number of {}: {}", if bytes { "bytes" } else { "lines" }, q(&raw)), 1);
    };
    let follow = o.flag('f') || o.flag('F') || o.long("follow");
    let interval = o
        .value('s')
        .or(o.long_value("sleep-interval"))
        .and_then(|s| s.parse::<f64>().ok())
        .map(Duration::from_secs_f64)
        .unwrap_or(Duration::from_millis(250));
    let files = inputs(&o.operands);
    let headers = (files.len() > 1 || o.flag('v') || o.long("verbose")) && !(o.flag('q') || o.long("quiet") || o.long("silent"));
    let mut status = 0;
    let mut followed: Vec<(String, Option<File>, u64)> = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if headers {
            let name = if f == "-" { "standard input" } else { f };
            writeln!(ctx.stdout, "{}==> {name} <==", if i > 0 { "\n" } else { "" })?;
        }
        let ok = with_input(ctx, f, |ctx, r| tail_stream(ctx, r, bytes, count))?;
        if !ok {
            status = 1;
        }
        if follow && f != "-" {
            let file = File::open(ctx.path(f)).ok();
            let pos = file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(0);
            followed.push((f.clone(), file, pos));
        }
    }
    if followed.is_empty() {
        return Ok(status);
    }
    ctx.stdout.flush()?;
    let mut last_shown = followed.len() - 1;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut slept = Duration::ZERO;
        while slept < interval {
            ctx.check_interrupt()?;
            let step = Duration::from_millis(50).min(interval - slept);
            std::thread::sleep(step);
            slept += step;
        }
        for (k, (name, file, pos)) in followed.iter_mut().enumerate() {
            if file.is_none() {
                *file = File::open(ctx.path(name)).ok();
                *pos = 0;
            }
            let Some(fh) = file else { continue };
            let len = match fh.metadata() {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            if len < *pos {
                ctx.err(format!("{name}: file truncated"));
                *pos = 0;
            }
            if len == *pos {
                continue;
            }
            fh.seek(SeekFrom::Start(*pos))?;
            if headers && k != last_shown {
                write!(ctx.stdout, "\n==> {name} <==\n")?;
                last_shown = k;
            }
            loop {
                let n = fh.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                *pos += n as u64;
                ctx.stdout.write_all(&buf[..n])?;
            }
            ctx.stdout.flush()?;
        }
    }
}

// ---------------------------------------------------------------------------
// wc

#[derive(Default, Clone, Copy)]
struct Counts {
    lines: u64,
    words: u64,
    chars: u64,
    bytes: u64,
    maxlen: u64,
}

fn count_stream(ctx: &Ctx, r: &mut dyn BufRead) -> io::Result<Counts> {
    let mut c = Counts::default();
    let mut in_word = false;
    let mut cur_len = 0u64;
    loop {
        ctx.check_interrupt()?;
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let n = buf.len();
        for &b in buf {
            c.bytes += 1;
            // chars: count UTF-8 lead bytes (and invalid bytes)
            if b & 0xC0 != 0x80 {
                c.chars += 1;
            }
            match b {
                b'\n' => {
                    c.lines += 1;
                    c.maxlen = c.maxlen.max(cur_len);
                    cur_len = 0;
                }
                b'\t' => cur_len += 8 - cur_len % 8,
                _ if b & 0xC0 != 0x80 => cur_len += 1,
                _ => {}
            }
            let space = matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
            if space {
                in_word = false;
            } else if !in_word && (b & 0xC0 != 0x80) {
                in_word = true;
                c.words += 1;
            }
        }
        r.consume(n);
    }
    c.maxlen = c.maxlen.max(cur_len);
    Ok(c)
}

pub fn wc(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "lwcmL", &["lines", "words", "bytes", "chars", "max-line-length"]);
    let mut sel = [
        o.flag('l') || o.long("lines"),
        o.flag('w') || o.long("words"),
        o.flag('m') || o.long("chars"),
        o.flag('c') || o.long("bytes"),
        o.flag('L') || o.long("max-line-length"),
    ];
    if !sel.iter().any(|&s| s) {
        sel = [true, true, false, true, false];
    }
    let files = inputs(&o.operands);
    let mut results: Vec<(Option<String>, Counts)> = Vec::new();
    let mut status = 0;
    let mut stdin_used = false;
    for f in &files {
        let mut got = Counts::default();
        let ok = with_input(ctx, f, |ctx, r| {
            got = count_stream(ctx, r)?;
            Ok(())
        })?;
        if f == "-" {
            stdin_used = true;
        }
        if ok {
            results.push((if o.operands.is_empty() { None } else { Some(f.clone()) }, got));
        } else {
            status = 1;
        }
    }
    let mut total = Counts::default();
    for (_, c) in &results {
        total.lines += c.lines;
        total.words += c.words;
        total.chars += c.chars;
        total.bytes += c.bytes;
        total.maxlen = total.maxlen.max(c.maxlen);
    }
    let fields = |c: &Counts| -> Vec<u64> {
        let all = [c.lines, c.words, c.chars, c.bytes, c.maxlen];
        all.iter().zip(sel.iter()).filter(|(_, s)| **s).map(|(v, _)| *v).collect()
    };
    let ncols = sel.iter().filter(|&&s| s).count();
    let width = if ncols == 1 && results.len() <= 1 {
        1
    } else {
        let maxv = fields(&total).into_iter().max().unwrap_or(0);
        let digits = maxv.to_string().len();
        if stdin_used { digits.max(7) } else { digits }
    };
    let print = |ctx: &mut Ctx, c: &Counts, name: Option<&str>| -> io::Result<()> {
        let mut parts: Vec<String> = fields(c).iter().map(|v| format!("{v:>width$}")).collect();
        if let Some(n) = name {
            parts.push(n.to_string());
        }
        writeln!(ctx.stdout, "{}", parts.join(" "))
    };
    for (name, c) in &results {
        print(ctx, c, name.as_deref())?;
    }
    if files.len() > 1 {
        print(ctx, &total, Some("total"))?;
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// sort

#[derive(Clone, Copy, Default, PartialEq)]
enum KeyKind {
    #[default]
    Str,
    Num,
    Human,
    Version,
}

#[derive(Clone, Default)]
struct Key {
    start_field: usize,
    start_char: usize,
    end_field: Option<usize>,
    end_char: usize,
    kind: KeyKind,
    fold: bool,
    reverse: bool,
    blanks: bool,
    has_opts: bool,
}

fn parse_key(s: &str) -> Option<Key> {
    let mut k = Key::default();
    let (a, b) = match s.split_once(',') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    };
    fn pos(p: &str, k: &mut Key) -> Option<(usize, usize)> {
        let end = p.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(p.len());
        let (num, opts) = p.split_at(end);
        for c in opts.chars() {
            k.has_opts = true;
            match c {
                'n' => k.kind = KeyKind::Num,
                'h' => k.kind = KeyKind::Human,
                'V' => k.kind = KeyKind::Version,
                'g' => k.kind = KeyKind::Num,
                'f' => k.fold = true,
                'r' => k.reverse = true,
                'b' => k.blanks = true,
                _ => return None,
            }
        }
        let (f, c) = match num.split_once('.') {
            Some((f, c)) => (f.parse().ok()?, c.parse().ok()?),
            None => (num.parse().ok()?, 0),
        };
        Some((f, c))
    }
    let (sf, sc) = pos(a, &mut k)?;
    if sf == 0 {
        return None;
    }
    k.start_field = sf;
    k.start_char = sc;
    if let Some(b) = b {
        let (ef, ec) = pos(b, &mut k)?;
        k.end_field = Some(ef);
        k.end_char = ec;
    }
    Some(k)
}

/// Byte ranges of fields: with a separator, split on it; otherwise a field
/// is a run of non-blanks plus the blanks before it.
fn field_ranges(line: &[u8], sep: Option<u8>) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    match sep {
        Some(s) => {
            let mut start = 0;
            for (i, &b) in line.iter().enumerate() {
                if b == s {
                    v.push((start, i));
                    start = i + 1;
                }
            }
            v.push((start, line.len()));
        }
        None => {
            let mut i = 0;
            while i < line.len() {
                let start = i;
                while i < line.len() && (line[i] == b' ' || line[i] == b'\t') {
                    i += 1;
                }
                while i < line.len() && line[i] != b' ' && line[i] != b'\t' {
                    i += 1;
                }
                v.push((start, i));
            }
        }
    }
    v
}

fn key_slice<'a>(line: &'a [u8], k: &Key, sep: Option<u8>) -> &'a [u8] {
    let f = field_ranges(line, sep);
    let skip_blanks = |s: usize, e: usize| -> usize {
        let mut s = s;
        while s < e && (line[s] == b' ' || line[s] == b'\t') {
            s += 1;
        }
        s
    };
    let Some(&(fs, fe)) = f.get(k.start_field - 1) else { return &[] };
    let mut start = if k.blanks || sep.is_none() && k.start_char > 0 { skip_blanks(fs, fe) } else { fs };
    if k.start_char > 0 {
        start = (start + k.start_char - 1).min(fe);
    }
    let end = match k.end_field {
        None => line.len(),
        Some(ef) => match f.get(ef - 1) {
            None => line.len(),
            Some(&(es, ee)) => {
                if k.end_char == 0 {
                    ee
                } else {
                    let s = if k.blanks { skip_blanks(es, ee) } else { es };
                    (s + k.end_char).min(ee)
                }
            }
        },
    };
    if start >= end { &[] } else { &line[start..end] }
}

fn parse_num(s: &[u8]) -> f64 {
    let t = String::from_utf8_lossy(s);
    let t = t.trim_start();
    let mut end = 0;
    let b = t.as_bytes();
    if end < b.len() && (b[end] == b'-' || b[end] == b'+') {
        end += 1;
    }
    while end < b.len() && (b[end].is_ascii_digit() || b[end] == b'.') {
        end += 1;
    }
    t[..end].parse().unwrap_or(0.0)
}

fn parse_human(s: &[u8]) -> f64 {
    let t = String::from_utf8_lossy(s);
    let t = t.trim_start();
    let end = t.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(t.len());
    let v: f64 = t[..end].parse().unwrap_or(0.0);
    let mult = match t[end..].chars().next() {
        Some('K' | 'k') => 1e3,
        Some('M') => 1e6,
        Some('G') => 1e9,
        Some('T') => 1e12,
        Some('P') => 1e15,
        Some('E') => 1e18,
        _ => 1.0,
    };
    v * mult
}

fn version_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let da = a[i].is_ascii_digit();
        let db = b[j].is_ascii_digit();
        if da && db {
            let si = i;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            let sj = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let na = std::str::from_utf8(&a[si..i]).unwrap().trim_start_matches('0');
            let nb = std::str::from_utf8(&b[sj..j]).unwrap().trim_start_matches('0');
            let o = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if o != Ordering::Equal {
                return o;
            }
        } else {
            let o = a[i].cmp(&b[j]);
            if o != Ordering::Equal {
                return o;
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

fn cmp_key(a: &[u8], b: &[u8], kind: KeyKind, fold: bool) -> std::cmp::Ordering {
    match kind {
        KeyKind::Num => parse_num(a).partial_cmp(&parse_num(b)).unwrap_or(std::cmp::Ordering::Equal),
        KeyKind::Human => parse_human(a).partial_cmp(&parse_human(b)).unwrap_or(std::cmp::Ordering::Equal),
        KeyKind::Version => version_cmp(a, b),
        KeyKind::Str => {
            if fold {
                a.iter().map(|c| c.to_ascii_uppercase()).cmp(b.iter().map(|c| c.to_ascii_uppercase()))
            } else {
                a.cmp(b)
            }
        }
    }
}

pub fn sort(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "kto", long_with_value: &["key", "field-separator", "output"], allow_number: false, stop_at_operand: false };
    let o = opts!(
        ctx,
        args,
        &spec,
        "rnufkstohVbgcz",
        &["reverse", "numeric-sort", "unique", "ignore-case", "key", "field-separator", "stable", "output", "human-numeric-sort", "version-sort", "ignore-leading-blanks", "general-numeric-sort", "check", "zero-terminated"]
    );
    let mut global = Key { start_field: 1, ..Default::default() };
    if o.flag('n') || o.flag('g') || o.long("numeric-sort") || o.long("general-numeric-sort") {
        global.kind = KeyKind::Num;
    }
    if o.flag('h') || o.long("human-numeric-sort") {
        global.kind = KeyKind::Human;
    }
    if o.flag('V') || o.long("version-sort") {
        global.kind = KeyKind::Version;
    }
    global.fold = o.flag('f') || o.long("ignore-case");
    global.blanks = o.flag('b') || o.long("ignore-leading-blanks");
    let reverse = o.flag('r') || o.long("reverse");
    let unique = o.flag('u') || o.long("unique");
    let stable = o.flag('s') || o.long("stable");
    let sep = match o.value('t').or(o.long_value("field-separator")) {
        Some(t) if t.len() == 1 => Some(t.as_bytes()[0]),
        Some("\\t") => Some(b'\t'),
        Some("\\0") => Some(0),
        Some(t) => return usage(ctx, format!("multi-character tab {}", q(t)), 2),
        None => None,
    };
    let mut keys = Vec::new();
    for k in o.values('k').chain(o.long_value("key")) {
        match parse_key(k) {
            Some(mut key) => {
                if !key.has_opts {
                    key.kind = global.kind;
                    key.fold = global.fold;
                    key.blanks = global.blanks;
                    key.reverse = false;
                }
                keys.push(key);
            }
            None => return usage(ctx, format!("invalid key specification {}", q(k)), 2),
        }
    }
    let zero = o.flag('z') || o.long("zero-terminated");
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut status = 0;
    for f in inputs(&o.operands) {
        let ok = with_input(ctx, &f, |_ctx, r| {
            if zero {
                let mut all = Vec::new();
                r.read_to_end(&mut all)?;
                if all.last() == Some(&0) {
                    all.pop();
                }
                if !all.is_empty() {
                    lines.extend(all.split(|&b| b == 0).map(|s| s.to_vec()));
                }
            } else {
                lines.extend(read_lines(r)?);
            }
            Ok(())
        })?;
        if !ok {
            status = 2;
        }
    }
    let compare = |a: &Vec<u8>, b: &Vec<u8>| -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if keys.is_empty() {
            let (x, y): (&[u8], &[u8]) = if global.blanks {
                (key_slice(a, &global, sep), key_slice(b, &global, sep))
            } else {
                (a, b)
            };
            let o = cmp_key(x, y, global.kind, global.fold);
            if o != Ordering::Equal {
                return if reverse { o.reverse() } else { o };
            }
        } else {
            for k in &keys {
                let o = cmp_key(key_slice(a, k, sep), key_slice(b, k, sep), k.kind, k.fold);
                let o = if k.reverse != reverse { o.reverse() } else { o };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
        if stable || unique {
            Ordering::Equal
        } else {
            let o = a.cmp(b);
            if reverse { o.reverse() } else { o }
        }
    };
    if o.flag('c') || o.long("check") {
        for (i, w) in lines.windows(2).enumerate() {
            let bad = if unique { compare(&w[0], &w[1]) != std::cmp::Ordering::Less } else { compare(&w[0], &w[1]) == std::cmp::Ordering::Greater };
            if bad {
                ctx.err(format!("-:{}: disorder: {}", i + 2, String::from_utf8_lossy(&w[1])));
                return Ok(1);
            }
        }
        return Ok(status);
    }
    lines.sort_by(compare);
    if unique {
        lines.dedup_by(|b, a| compare(a, b) == std::cmp::Ordering::Equal);
    }
    let end = if zero { 0u8 } else { b'\n' };
    let mut out = Vec::new();
    for l in &lines {
        out.extend_from_slice(l);
        out.push(end);
    }
    match o.value('o').or(o.long_value("output")) {
        Some(path) => {
            if let Err(e) = std::fs::write(ctx.path(path), &out) {
                return usage(ctx, format!("open failed: {path}: {}", msg(&e)), 2);
            }
        }
        None => ctx.stdout.write_all(&out)?,
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// uniq

pub fn uniq(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "fsw", long_with_value: &["skip-fields", "skip-chars", "check-chars"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "cduifsw", &["count", "repeated", "unique", "ignore-case", "skip-fields", "skip-chars", "check-chars"]);
    let count = o.flag('c') || o.long("count");
    let dups = o.flag('d') || o.long("repeated");
    let uniques = o.flag('u') || o.long("unique");
    let icase = o.flag('i') || o.long("ignore-case");
    let num = |c: char, l: &str| o.value(c).or(o.long_value(l)).and_then(|v| v.parse::<usize>().ok());
    let skip_f = num('f', "skip-fields").unwrap_or(0);
    let skip_c = num('s', "skip-chars").unwrap_or(0);
    let check = num('w', "check-chars");
    if o.operands.len() > 2 {
        return usage(ctx, format!("extra operand {}", q(&o.operands[2])), 1);
    }
    let input = o.operands.first().cloned().unwrap_or("-".into());
    let mut out_file = match o.operands.get(1) {
        Some(p) => match File::create(ctx.path(p)) {
            Ok(f) => Some(io::BufWriter::new(f)),
            Err(e) => return usage(ctx, format!("{p}: {}", msg(&e)), 1),
        },
        None => None,
    };
    let key = |l: &[u8]| -> Vec<u8> {
        let mut i = 0;
        for _ in 0..skip_f {
            while i < l.len() && (l[i] == b' ' || l[i] == b'\t') {
                i += 1;
            }
            while i < l.len() && l[i] != b' ' && l[i] != b'\t' {
                i += 1;
            }
        }
        let s = &l[(i + skip_c).min(l.len())..];
        let s = match check {
            Some(w) => &s[..w.min(s.len())],
            None => s,
        };
        if icase { s.to_ascii_lowercase() } else { s.to_vec() }
    };
    let mut groups: Vec<(u64, Vec<u8>)> = Vec::new();
    let ok = with_input(ctx, &input, |_ctx, r| {
        let mut prev: Option<(u64, Vec<u8>, Vec<u8>)> = None;
        for l in read_lines(r)? {
            let k = key(&l);
            match &mut prev {
                Some((n, _, pk)) if *pk == k => *n += 1,
                _ => {
                    if let Some((n, line, _)) = prev.take() {
                        groups.push((n, line));
                    }
                    prev = Some((1, l, k));
                }
            }
        }
        if let Some((n, line, _)) = prev {
            groups.push((n, line));
        }
        Ok(())
    })?;
    let mut buf = Vec::new();
    for (n, line) in groups {
        if (dups && n < 2) || (uniques && n > 1) {
            continue;
        }
        if count {
            buf.extend_from_slice(format!("{n:>7} ").as_bytes());
        }
        buf.extend_from_slice(&line);
        buf.push(b'\n');
    }
    match &mut out_file {
        Some(f) => f.write_all(&buf)?,
        None => ctx.stdout.write_all(&buf)?,
    }
    Ok(if ok { 0 } else { 1 })
}

// ---------------------------------------------------------------------------
// cut

fn parse_list(s: &str) -> Option<Vec<(usize, usize)>> {
    let mut v = Vec::new();
    for part in s.split([',', ' ']) {
        if part.is_empty() {
            return None;
        }
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (
                if a.is_empty() { 1 } else { a.parse().ok()? },
                if b.is_empty() { usize::MAX } else { b.parse().ok()? },
            ),
            None => {
                let n = part.parse().ok()?;
                (n, n)
            }
        };
        if a == 0 || b < a {
            return None;
        }
        v.push((a, b));
    }
    Some(v)
}

fn in_list(list: &[(usize, usize)], i: usize) -> bool {
    list.iter().any(|&(a, b)| i >= a && i <= b)
}

pub fn cut(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "bcfd", long_with_value: &["bytes", "characters", "fields", "delimiter", "output-delimiter"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "bcfdsn", &["bytes", "characters", "fields", "delimiter", "output-delimiter", "only-delimited", "complement"]);
    let modes = [
        ('b', o.value('b').or(o.long_value("bytes"))),
        ('c', o.value('c').or(o.long_value("characters"))),
        ('f', o.value('f').or(o.long_value("fields"))),
    ];
    let chosen: Vec<_> = modes.iter().filter(|(_, v)| v.is_some()).collect();
    if chosen.len() != 1 {
        return usage(ctx, if chosen.is_empty() { "you must specify a list of bytes, characters, or fields" } else { "only one type of list may be specified" }, 1);
    }
    let (mode, list_s) = (chosen[0].0, chosen[0].1.unwrap());
    let Some(list) = parse_list(list_s) else {
        return usage(ctx, format!("invalid field range {}", q(list_s)), 1);
    };
    let complement = o.long("complement");
    let delim = match o.value('d').or(o.long_value("delimiter")) {
        Some(d) => {
            let mut c = d.chars();
            match (c.next(), c.next()) {
                (Some(ch), None) => ch.to_string(),
                (None, _) => "\0".to_string(),
                _ => return usage(ctx, "the delimiter must be a single character", 1),
            }
        }
        None => "\t".to_string(),
    };
    let out_delim = o.long_value("output-delimiter").map(String::from).unwrap_or_else(|| if mode == 'f' { delim.clone() } else { String::new() });
    let only_delim = o.flag('s') || o.long("only-delimited");
    let sel = |i: usize| in_list(&list, i) != complement;
    let mut status = 0;
    for f in inputs(&o.operands) {
        let ok = with_input(ctx, &f, |ctx, r| {
            let mut line = Vec::new();
            while read_line(r, &mut line)? {
                ctx.check_interrupt()?;
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                let mut out: Vec<u8> = Vec::new();
                match mode {
                    'b' => {
                        let mut last: Option<usize> = None;
                        for (i, &b) in line.iter().enumerate() {
                            if sel(i + 1) {
                                if !out_delim.is_empty() && last.is_some_and(|l| l + 1 != i) {
                                    out.extend_from_slice(out_delim.as_bytes());
                                }
                                out.push(b);
                                last = Some(i);
                            }
                        }
                    }
                    'c' => {
                        let s = String::from_utf8_lossy(&line);
                        let mut last: Option<usize> = None;
                        for (i, ch) in s.chars().enumerate() {
                            if sel(i + 1) {
                                if !out_delim.is_empty() && last.is_some_and(|l| l + 1 != i) {
                                    out.extend_from_slice(out_delim.as_bytes());
                                }
                                let mut tmp = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                                last = Some(i);
                            }
                        }
                    }
                    _ => {
                        let s = String::from_utf8_lossy(&line);
                        if !s.contains(delim.as_str()) {
                            if !only_delim {
                                out.extend_from_slice(&line);
                                out.push(b'\n');
                                ctx.stdout.write_all(&out)?;
                            }
                            continue;
                        }
                        let picked: Vec<&str> = s.split(delim.as_str()).enumerate().filter(|(i, _)| sel(i + 1)).map(|(_, f)| f).collect();
                        out.extend_from_slice(picked.join(&out_delim).as_bytes());
                    }
                }
                out.push(b'\n');
                ctx.stdout.write_all(&out)?;
            }
            Ok(())
        })?;
        if !ok {
            status = 1;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// tr

fn expand_set(s: &str) -> Result<Vec<char>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let esc = |chars: &[char], i: &mut usize| -> char {
        // chars[*i] == '\\'
        *i += 1;
        let Some(&c) = chars.get(*i) else { return '\\' };
        *i += 1;
        match c {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            'a' => '\x07',
            'b' => '\x08',
            'f' => '\x0c',
            'v' => '\x0b',
            '0'..='7' => {
                let mut v = c.to_digit(8).unwrap();
                let mut k = 0;
                while k < 2 && *i < chars.len() && chars[*i].is_digit(8) {
                    v = v * 8 + chars[*i].to_digit(8).unwrap();
                    *i += 1;
                    k += 1;
                }
                char::from_u32(v).unwrap_or('\0')
            }
            other => other,
        }
    };
    while i < chars.len() {
        if chars[i] == '[' && chars.get(i + 1) == Some(&':')
            && let Some(end) = s[s.char_indices().nth(i + 2).map(|x| x.0).unwrap_or(s.len())..].find(":]") {
                let name: String = chars[i + 2..].iter().collect::<String>()[..end].to_string();
                let class: Vec<char> = (0u8..128).map(|b| b as char).filter(|&c| char_class(&name, c)).collect();
                if class.is_empty() {
                    return Err(format!("invalid character class {}", q(&name)));
                }
                out.extend(class);
                i += 2 + name.chars().count() + 2;
                continue;
            }
        let c = if chars[i] == '\\' {
            esc(&chars, &mut i)
        } else {
            i += 1;
            chars[i - 1]
        };
        if i < chars.len() && chars[i] == '-' && i + 1 < chars.len() {
            i += 1;
            let hi = if chars[i] == '\\' {
                esc(&chars, &mut i)
            } else {
                i += 1;
                chars[i - 1]
            };
            if hi < c {
                return Err(format!("range-endpoints of '{c}-{hi}' are in reverse collating sequence order"));
            }
            out.extend(c..=hi);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

pub fn tr(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "dscCt", &["delete", "squeeze-repeats", "complement", "truncate-set1"]);
    let delete = o.flag('d') || o.long("delete");
    let squeeze = o.flag('s') || o.long("squeeze-repeats");
    let complement = o.flag('c') || o.flag('C') || o.long("complement");
    let need = if delete && !squeeze { 1 } else if !delete && !squeeze { 2 } else { 1 };
    if o.operands.len() < need {
        return usage(ctx, if o.operands.is_empty() { "missing operand".to_string() } else { format!("missing operand after {}", q(&o.operands[0])) }, 1);
    }
    let set1 = match expand_set(&o.operands[0]) {
        Ok(s) => s,
        Err(e) => return usage(ctx, e, 1),
    };
    let set2 = match o.operands.get(1).map(|s| expand_set(s)) {
        Some(Ok(s)) => Some(s),
        Some(Err(e)) => return usage(ctx, e, 1),
        None => None,
    };
    let s1 = set1;
    let in1 = |c: char| s1.contains(&c) != complement;
    let translate = !delete && set2.is_some();
    let map = |c: char| -> char {
        let Some(s2) = &set2 else { return c };
        if s2.is_empty() {
            return c;
        }
        if complement {
            return if in1(c) { *s2.last().unwrap() } else { c };
        }
        match s1.iter().rposition(|&x| x == c) {
            Some(i) => *s2.get(i).unwrap_or(s2.last().unwrap()),
            None => c,
        }
    };
    // the set whose repeats are squeezed
    let squeeze_set: Vec<char> = if delete || !translate { if complement { Vec::new() } else { s1.clone() } } else { set2.clone().unwrap_or_default() };
    let squeeze_complement = complement && (delete || !translate);
    let in_squeeze = |c: char| {
        if squeeze_complement { !s1.contains(&c) } else { squeeze_set.contains(&c) }
    };
    let mut last: Option<char> = None;
    let mut line = Vec::new();
    let mut out = String::new();
    loop {
        ctx.check_interrupt()?;
        if !read_line(ctx.stdin, &mut line)? {
            break;
        }
        out.clear();
        for c in String::from_utf8_lossy(&line).chars() {
            if delete && in1(c) {
                continue;
            }
            let c = if translate { map(c) } else { c };
            if squeeze && last == Some(c) && in_squeeze(c) {
                continue;
            }
            last = Some(c);
            out.push(c);
        }
        ctx.stdout.write_all(out.as_bytes())?;
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// tee

pub fn tee(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "ai", &["append", "ignore-interrupts"]);
    let append = o.flag('a') || o.long("append");
    let mut status = 0;
    let mut files: Vec<(String, File)> = Vec::new();
    for n in &o.operands {
        let r = std::fs::OpenOptions::new().write(true).create(true).append(append).truncate(!append).open(ctx.path(n));
        match r {
            Ok(f) => files.push((n.clone(), f)),
            Err(e) => {
                ctx.err(format!("{n}: {}", msg(&e)));
                status = 1;
            }
        }
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        ctx.check_interrupt()?;
        let n = match ctx.stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        ctx.stdout.write_all(&buf[..n])?;
        ctx.stdout.flush()?;
        let mut failed = Vec::new();
        for (i, (name, f)) in files.iter_mut().enumerate() {
            if let Err(e) = f.write_all(&buf[..n]) {
                ctx.err(format!("{name}: {}", msg(&e)));
                failed.push(i);
            }
        }
        for i in failed.into_iter().rev() {
            files.remove(i);
            status = 1;
        }
    }
    Ok(status)
}

// ---------------------------------------------------------------------------
// nl, rev, tac, paste

pub fn nl(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "bwsvi", long_with_value: &["body-numbering", "number-width", "number-separator", "starting-line-number", "line-increment"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "bwsvi", &["body-numbering", "number-width", "number-separator", "starting-line-number", "line-increment"]);
    let style = o.value('b').or(o.long_value("body-numbering")).unwrap_or("t").to_string();
    if !matches!(style.as_str(), "a" | "t" | "n") {
        return usage(ctx, format!("invalid body numbering style: {}", q(&style)), 1);
    }
    let width: usize = o.value('w').or(o.long_value("number-width")).and_then(|v| v.parse().ok()).unwrap_or(6);
    let sep = o.value('s').or(o.long_value("number-separator")).unwrap_or("\t").to_string();
    let mut n: i64 = o.value('v').or(o.long_value("starting-line-number")).and_then(|v| v.parse().ok()).unwrap_or(1);
    let inc: i64 = o.value('i').or(o.long_value("line-increment")).and_then(|v| v.parse().ok()).unwrap_or(1);
    let blank_prefix = " ".repeat(width + sep.chars().count());
    let mut status = 0;
    for f in inputs(&o.operands) {
        let ok = with_input(ctx, &f, |ctx, r| {
            let mut line = Vec::new();
            while read_line(r, &mut line)? {
                ctx.check_interrupt()?;
                let empty = line == b"\n" || line.is_empty();
                let number = match style.as_str() {
                    "a" => true,
                    "t" => !empty,
                    _ => false,
                };
                if number {
                    write!(ctx.stdout, "{n:>width$}{sep}")?;
                    n += inc;
                } else {
                    ctx.stdout.write_all(blank_prefix.as_bytes())?;
                }
                ctx.stdout.write_all(&line)?;
            }
            Ok(())
        })?;
        if !ok {
            status = 1;
        }
    }
    Ok(status)
}

pub fn rev(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "", &[]);
    let mut status = 0;
    for f in inputs(&o.operands) {
        let ok = with_input(ctx, &f, |ctx, r| {
            let mut line = Vec::new();
            while read_line(r, &mut line)? {
                ctx.check_interrupt()?;
                let nl = line.last() == Some(&b'\n');
                let body = if nl { &line[..line.len() - 1] } else { &line[..] };
                let s: String = String::from_utf8_lossy(body).chars().rev().collect();
                ctx.stdout.write_all(s.as_bytes())?;
                if nl {
                    ctx.stdout.write_all(b"\n")?;
                }
            }
            Ok(())
        })?;
        if !ok {
            status = 1;
        }
    }
    Ok(status)
}

pub fn tac(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let o = opts!(ctx, args, &args::SIMPLE, "", &[]);
    let mut status = 0;
    for f in inputs(&o.operands) {
        let ok = with_input(ctx, &f, |ctx, r| {
            let mut all = Vec::new();
            r.read_to_end(&mut all)?;
            let mut lines: Vec<&[u8]> = all.split_inclusive(|&b| b == b'\n').collect();
            lines.reverse();
            for l in lines {
                ctx.stdout.write_all(l)?;
                if l.last() != Some(&b'\n') {
                    ctx.stdout.write_all(b"\n")?;
                }
            }
            Ok(())
        })?;
        if !ok {
            status = 1;
        }
    }
    Ok(status)
}

pub fn paste(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let spec = Spec { with_value: "d", long_with_value: &["delimiters"], allow_number: false, stop_at_operand: false };
    let o = opts!(ctx, args, &spec, "ds", &["delimiters", "serial"]);
    let dspec = o.value('d').or(o.long_value("delimiters")).unwrap_or("\t");
    let mut delims: Vec<String> = Vec::new();
    let mut it = dspec.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => delims.push("\n".into()),
                Some('t') => delims.push("\t".into()),
                Some('0') => delims.push(String::new()),
                Some('\\') => delims.push("\\".into()),
                Some(x) => delims.push(x.to_string()),
                None => delims.push("\\".into()),
            }
        } else {
            delims.push(c.to_string());
        }
    }
    if delims.is_empty() {
        delims.push(String::new());
    }
    let mut status = 0;
    let mut cols: Vec<Vec<Vec<u8>>> = Vec::new();
    for f in inputs(&o.operands) {
        let mut lines = Vec::new();
        let ok = with_input(ctx, &f, |_ctx, r| {
            lines = read_lines(r)?;
            Ok(())
        })?;
        if !ok {
            status = 1;
        }
        cols.push(lines);
    }
    let mut out = Vec::new();
    if o.flag('s') || o.long("serial") {
        for c in &cols {
            for (i, l) in c.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(delims[(i - 1) % delims.len()].as_bytes());
                }
                out.extend_from_slice(l);
            }
            out.push(b'\n');
        }
    } else {
        let rows = cols.iter().map(|c| c.len()).max().unwrap_or(0);
        for r in 0..rows {
            for (i, c) in cols.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(delims[(i - 1) % delims.len()].as_bytes());
                }
                if let Some(l) = c.get(r) {
                    out.extend_from_slice(l);
                }
            }
            out.push(b'\n');
        }
    }
    ctx.stdout.write_all(&out)?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use crate::utils::common::testutil::TempDir;
    use crate::utils::run_capture;
    use std::path::Path;

    fn r(name: &str, args: &[&str], input: &str) -> String {
        run_capture(name, args, input.as_bytes(), Path::new(".")).1
    }

    #[test]
    fn cat_flags() {
        assert_eq!(r("cat", &[], "a\nb\n"), "a\nb\n");
        assert_eq!(r("cat", &["-n"], "a\n\nb\n"), "     1\ta\n     2\t\n     3\tb\n");
        assert_eq!(r("cat", &["-b"], "a\n\nb\n"), "     1\ta\n\n     2\tb\n");
        assert_eq!(r("cat", &["-s"], "a\n\n\n\nb\n"), "a\n\nb\n");
        assert_eq!(r("cat", &["-E"], "a\n"), "a$\n");
        assert_eq!(r("cat", &["-A"], "a\tb\x01\n"), "a^Ib^A$\n");
        let d = TempDir::new("cat");
        d.write("x", "X\n");
        d.write("y", "Y\n");
        let (s, out, _) = run_capture("cat", &["x", "-", "y"], b"S\n", d.path());
        assert_eq!((s, out.as_str()), (0, "X\nS\nY\n"));
        let (s, out, e) = run_capture("cat", &["x", "nope"], b"", d.path());
        assert_eq!((s, out.as_str(), e.as_str()), (1, "X\n", "cat: nope: No such file or directory\n"));
        let (s, _, e) = run_capture("cat", &["."], b"", d.path());
        assert_eq!((s, e.as_str()), (1, "cat: .: Is a directory\n"));
    }

    #[test]
    fn head_tail() {
        let input: String = (1..=15).map(|i| format!("{i}\n")).collect();
        assert_eq!(r("head", &[], &input).lines().count(), 10);
        assert_eq!(r("head", &["-n", "2"], &input), "1\n2\n");
        assert_eq!(r("head", &["-3"], &input), "1\n2\n3\n");
        assert_eq!(r("head", &["-n", "-13"], &input), "1\n2\n");
        assert_eq!(r("head", &["-c", "3"], &input), "1\n2");
        assert_eq!(r("head", &["-c", "-3"], "abcdef"), "abc");
        assert_eq!(r("tail", &["-n", "2"], &input), "14\n15\n");
        assert_eq!(r("tail", &["-2"], &input), "14\n15\n");
        assert_eq!(r("tail", &["-n", "+14"], &input), "14\n15\n");
        assert_eq!(r("tail", &["-c", "3"], &input), "15\n");
        assert_eq!(r("tail", &["-c", "+3"], "abcdef"), "cdef");
        assert_eq!(r("tail", &[], &input).lines().count(), 10);
        let d = TempDir::new("head");
        d.write("a", "1\n2\n");
        d.write("b", "3\n");
        let (_, out, _) = run_capture("head", &["-n1", "a", "b"], b"", d.path());
        assert_eq!(out, "==> a <==\n1\n\n==> b <==\n3\n");
        let (_, out, _) = run_capture("tail", &["-q", "-n1", "a", "b"], b"", d.path());
        assert_eq!(out, "2\n3\n");
        assert_eq!(run_capture("head", &["-n", "x"], b"", d.path()).0, 1);
    }

    #[test]
    fn tail_follow_interrupt_free() {
        // -f on stdin only: returns immediately (nothing to follow)
        assert_eq!(r("tail", &["-f", "-n1"], "a\nb\n"), "b\n");
    }

    #[test]
    fn wc_counts() {
        assert_eq!(r("wc", &[], "hello world\nfoo\n"), "      2       3      16\n");
        assert_eq!(r("wc", &["-l"], "a\nb\n"), "2\n");
        assert_eq!(r("wc", &["-w"], "a b  c\n"), "3\n");
        assert_eq!(r("wc", &["-c"], "héllo"), "6\n");
        assert_eq!(r("wc", &["-m"], "héllo"), "5\n");
        assert_eq!(r("wc", &["-L"], "ab\nabcd\n"), "4\n");
        let d = TempDir::new("wc");
        d.write("a", "1 2\n");
        d.write("b", "x\ny\n");
        let (_, out, _) = run_capture("wc", &["a", "b"], b"", d.path());
        assert_eq!(out, "1 2 4 a\n2 2 4 b\n3 4 8 total\n");
        let (_, out, _) = run_capture("wc", &["-l", "a"], b"", d.path());
        assert_eq!(out, "1 a\n");
    }

    #[test]
    fn sort_modes() {
        assert_eq!(r("sort", &[], "b\na\nc\n"), "a\nb\nc\n");
        assert_eq!(r("sort", &["-r"], "b\na\nc\n"), "c\nb\na\n");
        assert_eq!(r("sort", &["-n"], "10\n9\n-1\n100\n"), "-1\n9\n10\n100\n");
        assert_eq!(r("sort", &["-u"], "b\na\nb\n"), "a\nb\n");
        assert_eq!(r("sort", &["-f"], "b\nA\na\nB\n"), "A\na\nB\nb\n");
        assert_eq!(r("sort", &["-t", ",", "-k2"], "x,3\ny,1\nz,2\n"), "y,1\nz,2\nx,3\n");
        assert_eq!(r("sort", &["-k2n"], "a 10\nb 9\nc 100\n"), "b 9\na 10\nc 100\n");
        assert_eq!(r("sort", &["-k2,2nr", "-k1,1"], "b 1\na 1\nc 2\n"), "c 2\na 1\nb 1\n");
        assert_eq!(r("sort", &["-h"], "1G\n10K\n5M\n"), "10K\n5M\n1G\n");
        assert_eq!(r("sort", &["-V"], "v1.10\nv1.2\nv1.9\n"), "v1.2\nv1.9\nv1.10\n");
        assert_eq!(r("sort", &["-s", "-k1,1"], "a 2\nb 1\na 1\n"), "a 2\na 1\nb 1\n");
        assert_eq!(run_capture("sort", &["-c"], b"a\nc\nb\n", Path::new(".")).0, 1);
        let d = TempDir::new("sort");
        d.write("f", "z\ny\n");
        assert_eq!(run_capture("sort", &["-o", "f", "f"], b"", d.path()).0, 0);
        assert_eq!(d.read("f"), "y\nz\n");
    }

    #[test]
    fn uniq_modes() {
        let input = "a\na\nb\nc\nc\nc\n";
        assert_eq!(r("uniq", &[], input), "a\nb\nc\n");
        assert_eq!(r("uniq", &["-c"], input), "      2 a\n      1 b\n      3 c\n");
        assert_eq!(r("uniq", &["-d"], input), "a\nc\n");
        assert_eq!(r("uniq", &["-u"], input), "b\n");
        assert_eq!(r("uniq", &["-i"], "A\na\nb\n"), "A\nb\n");
        assert_eq!(r("uniq", &["-f", "1"], "x a\ny a\nz b\n"), "x a\nz b\n");
    }

    #[test]
    fn cut_modes() {
        assert_eq!(r("cut", &["-d", ":", "-f", "1,3"], "a:b:c:d\n"), "a:c\n");
        assert_eq!(r("cut", &["-d:", "-f2-"], "a:b:c\n"), "b:c\n");
        assert_eq!(r("cut", &["-c", "2-3"], "héllo\n"), "él\n");
        assert_eq!(r("cut", &["-b", "1,3"], "abc\n"), "ac\n");
        assert_eq!(r("cut", &["-f", "2"], "a\tb\nnodelim\n"), "b\nnodelim\n");
        assert_eq!(r("cut", &["-s", "-f", "2"], "a\tb\nnodelim\n"), "b\n");
        assert_eq!(r("cut", &["-d", ",", "-f", "1,2", "--output-delimiter", "|"], "a,b,c\n"), "a|b\n");
        assert_eq!(r("cut", &["-d", ",", "--complement", "-f", "2"], "a,b,c\n"), "a,c\n");
        assert_eq!(run_capture("cut", &["-f", "0"], b"", Path::new(".")).0, 1);
    }

    #[test]
    fn tr_modes() {
        assert_eq!(r("tr", &["a-z", "A-Z"], "hello\n"), "HELLO\n");
        assert_eq!(r("tr", &["[:lower:]", "[:upper:]"], "abc\n"), "ABC\n");
        assert_eq!(r("tr", &["-d", "l"], "hello\n"), "heo\n");
        assert_eq!(r("tr", &["-s", " "], "a   b  c\n"), "a b c\n");
        assert_eq!(r("tr", &["-cd", "[:digit:]\\n"], "a1b2c3\n"), "123\n");
        assert_eq!(r("tr", &["abc", "x"], "aabbcc"), "xxxxxx");
        assert_eq!(r("tr", &["-s", "a-z", "A-Z"], "aabbcc"), "ABC");
        assert_eq!(r("tr", &["\\n", " "], "a\nb\n"), "a b ");
        assert_eq!(r("tr", &["-c", "a", "_"], "abc"), "a__");
    }

    #[test]
    fn tee_writes() {
        let d = TempDir::new("tee");
        let (s, out, _) = run_capture("tee", &["o1", "o2"], b"data\n", d.path());
        assert_eq!((s, out.as_str()), (0, "data\n"));
        assert_eq!(d.read("o1"), "data\n");
        run_capture("tee", &["-a", "o1"], b"more\n", d.path());
        assert_eq!(d.read("o1"), "data\nmore\n");
    }

    #[test]
    fn small_ones() {
        assert_eq!(r("nl", &[], "a\n\nb\n"), "     1\ta\n       \n     2\tb\n");
        assert_eq!(r("nl", &["-b", "a", "-w", "2", "-s", " "], "a\n\n"), " 1 a\n 2 \n");
        assert_eq!(r("rev", &[], "abc\nhé\n"), "cba\néh\n");
        assert_eq!(r("tac", &[], "1\n2\n3\n"), "3\n2\n1\n");
        assert_eq!(r("tac", &[], "1\n2"), "2\n1\n");
        let d = TempDir::new("paste");
        d.write("a", "1\n2\n3\n");
        d.write("b", "x\ny\n");
        let (_, out, _) = run_capture("paste", &["a", "b"], b"", d.path());
        assert_eq!(out, "1\tx\n2\ty\n3\t\n");
        let (_, out, _) = run_capture("paste", &["-d", ",", "a", "b"], b"", d.path());
        assert_eq!(out, "1,x\n2,y\n3,\n");
        let (_, out, _) = run_capture("paste", &["-s", "a", "b"], b"", d.path());
        assert_eq!(out, "1\t2\t3\nx\ty\n");
        assert_eq!(r("paste", &["-s", "-d", ","], "a\nb\nc\n"), "a,b,c\n");
    }
}
