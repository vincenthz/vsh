//! `printf` and the C-style number formatting it shares with `seq -f`.

use std::io;

use super::Ctx;
use super::common::unescape;

#[derive(Default, Clone, Copy)]
pub struct Spec {
    pub left: bool,
    pub plus: bool,
    pub space: bool,
    pub alt: bool,
    pub zero: bool,
    pub width: Option<usize>,
    pub prec: Option<usize>,
}

fn pad(s: String, spec: &Spec, numeric: bool) -> String {
    let w = spec.width.unwrap_or(0);
    let len = s.chars().count();
    if len >= w {
        return s;
    }
    let fill = w - len;
    if spec.left {
        format!("{s}{}", " ".repeat(fill))
    } else if spec.zero && numeric {
        // zeros go after the sign / 0x prefix
        let split = s
            .char_indices()
            .find(|&(_, c)| !matches!(c, '+' | '-' | ' '))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let split = if s[split..].starts_with("0x") || s[split..].starts_with("0X") { split + 2 } else { split };
        format!("{}{}{}", &s[..split], "0".repeat(fill), &s[split..])
    } else {
        format!("{}{s}", " ".repeat(fill))
    }
}

fn sign(neg: bool, spec: &Spec) -> &'static str {
    if neg {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    }
}

pub fn fmt_int(v: i128, conv: char, spec: &Spec) -> String {
    let neg = v < 0 && matches!(conv, 'd' | 'i');
    let mag: u128 = if matches!(conv, 'd' | 'i') {
        v.unsigned_abs()
    } else if v < 0 {
        (v as i64 as u64) as u128
    } else {
        v as u128
    };
    let mut digits = match conv {
        'o' => format!("{mag:o}"),
        'x' => format!("{mag:x}"),
        'X' => format!("{mag:X}"),
        _ => mag.to_string(),
    };
    if let Some(p) = spec.prec {
        if p == 0 && mag == 0 {
            digits.clear();
        }
        while digits.len() < p {
            digits.insert(0, '0');
        }
    }
    let prefix = match conv {
        'o' if spec.alt && !digits.starts_with('0') => "0",
        'x' if spec.alt && mag != 0 => "0x",
        'X' if spec.alt && mag != 0 => "0X",
        _ => "",
    };
    let s = format!("{}{}{}", if matches!(conv, 'd' | 'i') { sign(neg, spec) } else { "" }, prefix, digits);
    let mut sp = *spec;
    if spec.prec.is_some() {
        sp.zero = false;
    }
    pad(s, &sp, true)
}

fn exp_form(v: f64, prec: usize, upper: bool) -> String {
    let s = format!("{:.*e}", prec, v);
    // Rust gives "1.5e2" / "1.5e-2"; C wants "1.5e+02".
    let (m, e) = s.split_once('e').unwrap();
    let e: i32 = e.parse().unwrap();
    let out = format!("{}e{}{:02}", m, if e < 0 { '-' } else { '+' }, e.abs());
    if upper { out.to_uppercase() } else { out }
}

fn strip_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let (m, e) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let m = m.trim_end_matches('0').trim_end_matches('.');
    format!("{m}{e}")
}

pub fn fmt_float(v: f64, conv: char, spec: &Spec) -> String {
    let upper = conv.is_ascii_uppercase();
    let neg = v.is_sign_negative() && !(v == 0.0 && !v.is_sign_negative());
    let a = v.abs();
    let body = if a.is_nan() {
        if upper { "NAN".into() } else { "nan".into() }
    } else if a.is_infinite() {
        if upper { "INF".into() } else { "inf".into() }
    } else {
        let prec = spec.prec.unwrap_or(6);
        match conv.to_ascii_lowercase() {
            'f' => {
                let s = format!("{:.*}", prec, a);
                if spec.alt && prec == 0 { format!("{s}.") } else { s }
            }
            'e' => exp_form(a, prec, upper),
            _ => {
                let p = if prec == 0 { 1 } else { prec };
                let x = if a == 0.0 {
                    0
                } else {
                    // exponent after rounding to p significant digits
                    let e = exp_form(a, p - 1, false);
                    e.split_once('e').unwrap().1.parse::<i32>().unwrap()
                };
                let s = if x < -4 || x >= p as i32 {
                    exp_form(a, p - 1, upper)
                } else {
                    format!("{:.*}", (p as i32 - 1 - x).max(0) as usize, a)
                };
                if spec.alt { s } else { strip_zeros(&s) }
            }
        }
    };
    let finite = a.is_finite();
    let s = format!("{}{}", sign(neg && !a.is_nan(), spec), body);
    pad(s, spec, finite)
}

/// Parse an integer argument like C printf: decimal, 0x hex, 0 octal,
/// or 'c / "c for a character code.
fn parse_int(s: &str) -> Option<i128> {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix('\'').or_else(|| t.strip_prefix('"')) {
        return Some(rest.chars().next().map(|c| c as i128).unwrap_or(0));
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let v = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i128::from_str_radix(h, 16).ok()?
    } else if body.len() > 1 && body.starts_with('0') {
        i128::from_str_radix(&body[1..], 8).ok()?
    } else {
        body.parse::<i128>().ok()?
    };
    Some(if neg { -v } else { v })
}

fn parse_float(s: &str) -> Option<f64> {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix('\'').or_else(|| t.strip_prefix('"')) {
        return Some(rest.chars().next().map(|c| c as u32 as f64).unwrap_or(0.0));
    }
    if let Some(i) = parse_int(t).filter(|_| t.contains(['x', 'X'])) {
        return Some(i as f64);
    }
    t.parse::<f64>().ok()
}

/// Shell-quote a string for `%q`.
fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=+,:@%".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn printf(ctx: &mut Ctx, args: &[String]) -> io::Result<i32> {
    let args = if args.first().map(|s| s.as_str()) == Some("--") { &args[1..] } else { args };
    let Some(format) = args.first() else {
        ctx.err("usage: printf format [arguments ...]");
        return Ok(1);
    };
    let fmt: Vec<char> = format.chars().collect();
    let rest = &args[1..];
    let mut ai = 0;
    let mut status = 0;
    let mut out: Vec<u8> = Vec::new();
    loop {
        let start_ai = ai;
        let mut i = 0;
        let mut stop = false;
        while i < fmt.len() {
            let c = fmt[i];
            if c == '\\' {
                // escapes in the format string
                let mut j = i + 1;
                if j < fmt.len() && ('0'..='7').contains(&fmt[j]) {
                    let st = j;
                    while j < fmt.len() && j - st < 3 && ('0'..='7').contains(&fmt[j]) {
                        j += 1;
                    }
                } else if j < fmt.len() && fmt[j] == 'x' {
                    j += 1;
                    let st = j;
                    while j < fmt.len() && j - st < 2 && fmt[j].is_ascii_hexdigit() {
                        j += 1;
                    }
                } else if j < fmt.len() && (fmt[j] == 'u' || fmt[j] == 'U') {
                    let max = if fmt[j] == 'u' { 4 } else { 8 };
                    j += 1;
                    let st = j;
                    while j < fmt.len() && j - st < max && fmt[j].is_ascii_hexdigit() {
                        j += 1;
                    }
                } else {
                    j = (j + 1).min(fmt.len());
                }
                let piece: String = fmt[i..j].iter().collect();
                let (bytes, _) = unescape(&piece, true);
                out.extend_from_slice(&bytes);
                i = j;
                continue;
            }
            if c != '%' {
                let mut tmp = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                i += 1;
                continue;
            }
            i += 1;
            if i < fmt.len() && fmt[i] == '%' {
                out.push(b'%');
                i += 1;
                continue;
            }
            let mut spec = Spec::default();
            while i < fmt.len() {
                match fmt[i] {
                    '-' => spec.left = true,
                    '+' => spec.plus = true,
                    ' ' => spec.space = true,
                    '#' => spec.alt = true,
                    '0' => spec.zero = true,
                    _ => break,
                }
                i += 1;
            }
            let next_arg = |ai: &mut usize| -> Option<String> {
                let a = rest.get(*ai).cloned();
                if a.is_some() {
                    *ai += 1;
                }
                a
            };
            if i < fmt.len() && fmt[i] == '*' {
                i += 1;
                let w = next_arg(&mut ai).and_then(|a| parse_int(&a)).unwrap_or(0);
                if w < 0 {
                    spec.left = true;
                }
                spec.width = Some(w.unsigned_abs() as usize);
            } else {
                let st = i;
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    i += 1;
                }
                if i > st {
                    spec.width = fmt[st..i].iter().collect::<String>().parse().ok();
                }
            }
            if i < fmt.len() && fmt[i] == '.' {
                i += 1;
                if i < fmt.len() && fmt[i] == '*' {
                    i += 1;
                    let p = next_arg(&mut ai).and_then(|a| parse_int(&a)).unwrap_or(0);
                    spec.prec = if p < 0 { None } else { Some(p as usize) };
                } else {
                    let st = i;
                    while i < fmt.len() && fmt[i].is_ascii_digit() {
                        i += 1;
                    }
                    spec.prec = Some(fmt[st..i].iter().collect::<String>().parse().unwrap_or(0));
                }
            }
            // length modifiers are accepted and ignored
            while i < fmt.len() && matches!(fmt[i], 'h' | 'l' | 'L' | 'j' | 'z' | 't') {
                i += 1;
            }
            let Some(&conv) = fmt.get(i) else {
                ctx.err("missing format character");
                return Ok(1);
            };
            i += 1;
            let arg = next_arg(&mut ai);
            let s = match conv {
                's' => {
                    let mut s = arg.unwrap_or_default();
                    if let Some(p) = spec.prec {
                        s = s.chars().take(p).collect();
                    }
                    pad(s, &spec, false)
                }
                'q' => pad(shell_quote(&arg.unwrap_or_default()), &spec, false),
                'b' => {
                    let (bytes, c) = unescape(&arg.unwrap_or_default(), false);
                    let mut s = String::from_utf8_lossy(&bytes).into_owned();
                    if let Some(p) = spec.prec {
                        s = s.chars().take(p).collect();
                    }
                    if c {
                        stop = true;
                    }
                    pad(s, &spec, false)
                }
                'c' => pad(arg.and_then(|a| a.chars().next()).map(String::from).unwrap_or_default(), &spec, false),
                'd' | 'i' | 'o' | 'u' | 'x' | 'X' => {
                    let v = match arg {
                        None => 0,
                        Some(a) => match parse_int(&a) {
                            Some(v) => v,
                            None => {
                                // accept a leading number, warn about the rest
                                ctx.err(format!("'{a}': invalid number"));
                                status = 1;
                                let t = a.trim();
                                let end = t
                                    .char_indices()
                                    .find(|&(k, c)| !(c.is_ascii_digit() || (k == 0 && (c == '-' || c == '+'))))
                                    .map(|(k, _)| k)
                                    .unwrap_or(t.len());
                                t[..end].parse().unwrap_or(0)
                            }
                        },
                    };
                    fmt_int(v, conv, &spec)
                }
                'e' | 'E' | 'f' | 'F' | 'g' | 'G' => {
                    let v = match arg {
                        None => 0.0,
                        Some(a) => match parse_float(&a) {
                            Some(v) => v,
                            None => {
                                ctx.err(format!("'{a}': invalid number"));
                                status = 1;
                                0.0
                            }
                        },
                    };
                    fmt_float(v, conv, &spec)
                }
                other => {
                    ctx.err(format!("%{other}: invalid conversion specification"));
                    return Ok(1);
                }
            };
            out.extend_from_slice(s.as_bytes());
            if stop {
                break;
            }
        }
        if stop || ai >= rest.len() || ai == start_ai {
            break;
        }
    }
    ctx.stdout.write_all(&out)?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use crate::utils::run_capture;
    use std::path::Path;

    fn p(args: &[&str]) -> String {
        run_capture("printf", args, b"", Path::new(".")).1
    }

    #[test]
    fn basics() {
        assert_eq!(p(&["%s-%s\\n", "a", "b", "c"]), "a-b\nc-\n");
        assert_eq!(p(&["%5d|%-5d|%05d\\n", "42", "42", "-42"]), "   42|42   |-0042\n");
        assert_eq!(p(&["%x %X %o %#x\\n", "255", "255", "8", "255"]), "ff FF 10 0xff\n");
        assert_eq!(p(&["%.2f %e %g %g\\n", "3.14159", "1234.5", "0.0001", "1e10"]), "3.14 1.234500e+03 0.0001 1e+10\n");
        assert_eq!(p(&["%g %g\\n", "100000", "1000000"]), "100000 1e+06\n");
        assert_eq!(p(&["%c%c\\n", "hello", "world"]), "hw\n");
        assert_eq!(p(&["%b\\n", "a\\tb"]), "a\tb\n");
        assert_eq!(p(&["%.3s|%*s\\n", "abcdef", "4", "x"]), "abc|   x\n");
        assert_eq!(p(&["%d %d\\n", "'A", "0x10"]), "65 16\n");
        assert_eq!(p(&["100%%\\n"]), "100%\n");
        assert_eq!(p(&["%s\\n"]), "\n");
        assert_eq!(p(&["\\101\\x42\\n"]), "AB\n");
        assert_eq!(p(&["%q\\n", "a b"]), "'a b'\n");
        assert_eq!(p(&["%+d % d\\n", "5", "5"]), "+5  5\n");
        assert_eq!(p(&["%b", "x\\cy", "z"]), "x");
    }

    #[test]
    fn invalid_number() {
        let (s, out, err) = run_capture("printf", &["%d\\n", "12abc"], b"", Path::new("."));
        assert_eq!(s, 1);
        assert_eq!(out, "12\n");
        assert!(err.contains("invalid number"));
    }
}
