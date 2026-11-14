//! Word expansion: braces, tilde, parameters, command substitution,
//! arithmetic, field splitting and filename generation.
//!
//! zsh semantics are followed where they differ from POSIX sh: unquoted
//! parameter values are neither split on IFS nor globbed (no SH_WORD_SPLIT,
//! no GLOB_SUBST), arrays expand to one word per element, an unquoted empty
//! parameter disappears, and a glob that matches nothing is an error.

use crate::arith;
use crate::ast::{Param, ParamOp, Part, Word};
use crate::glob;
use crate::shell::{Shell, Value, display_path};

#[derive(Debug)]
pub struct ExpandError(pub String);

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type XResult<T> = Result<T, ExpandError>;

fn xerr<T>(msg: impl Into<String>) -> XResult<T> {
    Err(ExpandError(msg.into()))
}

#[derive(Default, Clone)]
struct Field {
    text: String,
    /// The same text as a glob pattern (quoted characters escaped).
    pat: String,
    /// Contains an unquoted wildcard.
    glob: bool,
    /// Survives even if empty (something quoted was in it).
    keep: bool,
}

#[derive(Default)]
struct Fields {
    done: Vec<Field>,
    cur: Field,
    /// Single-word mode: arrays join, command substitutions don't split.
    single: bool,
}

impl Fields {
    fn push_lit(&mut self, s: &str) {
        self.cur.text.push_str(s);
        self.cur.pat.push_str(s);
        if glob::has_wildcards(s) {
            self.cur.glob = true;
        }
        if !s.is_empty() {
            self.cur.keep = true;
        }
    }

    fn push_text(&mut self, s: &str, quoted: bool) {
        self.cur.text.push_str(s);
        self.cur.pat.push_str(&glob::escape(s));
        if quoted || !s.is_empty() {
            self.cur.keep = true;
        }
    }

    fn split_here(&mut self) {
        let f = std::mem::take(&mut self.cur);
        self.done.push(f);
    }

    /// Several words: the first joins the current field, the last stays open.
    fn push_words(&mut self, words: &[String], quoted: bool) {
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                self.split_here();
            }
            self.push_text(w, quoted);
        }
    }

    fn push_split(&mut self, s: &str, ifs: &str) {
        let is_ifs = |c: char| ifs.contains(c);
        if s.is_empty() {
            return;
        }
        let starts_sep = s.starts_with(is_ifs);
        let words: Vec<&str> = s.split(is_ifs).filter(|w| !w.is_empty()).collect();
        if words.is_empty() {
            return;
        }
        if starts_sep && self.cur.keep {
            self.split_here();
        }
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                self.split_here();
            }
            self.push_text(w, false);
        }
        if s.ends_with(is_ifs) {
            self.split_here();
        }
    }

    fn finish(mut self) -> Vec<Field> {
        self.split_here();
        self.done.into_iter().filter(|f| f.keep).collect()
    }
}

enum PRes {
    Val(Option<Value>, bool),
    Word(Word),
}

impl Shell {
    /// Full expansion of command arguments.
    pub fn expand_args(&mut self, words: &[Word]) -> XResult<Vec<String>> {
        let mut out = Vec::new();
        for w in words {
            for w in brace_expand(w) {
                let w = self.tilde(&w, false);
                let mut f = Fields::default();
                self.expand_into(&w.0, &mut f, false)?;
                for field in f.finish() {
                    if field.glob && !self.opts.noglob {
                        let matches = glob::glob(&field.pat, &self.cwd, self.opts.globdots);
                        if matches.is_empty() {
                            if self.opts.nonomatch {
                                out.push(field.text);
                            } else {
                                return xerr(format!("no matches found: {}", field.text));
                            }
                        } else {
                            out.extend(matches);
                        }
                    } else {
                        out.push(field.text);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Expand to one string: no splitting, no globbing (assignments,
    /// redirection targets, here-documents, `case` words).
    pub fn expand_str(&mut self, w: &Word) -> XResult<String> {
        let w = self.tilde(w, false);
        let mut f = Fields { single: true, ..Default::default() };
        self.expand_into(&w.0, &mut f, false)?;
        Ok(f.cur.text)
    }

    /// Like `expand_str`, with tilde expansion after `:` too (`PATH=~/a:~/b`).
    pub fn expand_assign(&mut self, w: &Word) -> XResult<String> {
        let w = self.tilde(w, true);
        let mut f = Fields { single: true, ..Default::default() };
        self.expand_into(&w.0, &mut f, false)?;
        Ok(f.cur.text)
    }

    /// Expand to a pattern: wildcards only from unquoted literal text.
    pub fn expand_pattern(&mut self, w: &Word) -> XResult<String> {
        let mut f = Fields { single: true, ..Default::default() };
        self.expand_into(&w.0, &mut f, false)?;
        Ok(f.cur.pat)
    }

    fn expand_into(&mut self, parts: &[Part], f: &mut Fields, quoted: bool) -> XResult<()> {
        for part in parts {
            match part {
                Part::Lit(s) => {
                    if quoted {
                        f.push_text(s, true);
                    } else {
                        f.push_lit(s);
                    }
                }
                Part::Quoted(s) => f.push_text(s, true),
                Part::Double(inner) => {
                    f.cur.keep = true;
                    self.expand_into(inner, f, true)?;
                }
                Part::Param(p) => match self.param(p)? {
                    PRes::Word(w) => {
                        let w = self.tilde(&w, false);
                        self.expand_into(&w.0, f, quoted)?;
                    }
                    PRes::Val(None, _) => {}
                    PRes::Val(Some(Value::Str(s)), _) => f.push_text(&s, quoted),
                    PRes::Val(Some(Value::Array(a)), splat) => {
                        if f.single || (quoted && !splat) {
                            let sep = self.ifs().chars().next().map(String::from).unwrap_or_default();
                            f.push_text(&a.join(&sep), quoted);
                        } else if quoted {
                            f.push_words(&a, true);
                        } else {
                            let a: Vec<String> = a.into_iter().filter(|s| !s.is_empty()).collect();
                            f.push_words(&a, false);
                        }
                    }
                },
                Part::CmdSub(list) => {
                    let out = self.capture(list);
                    let out = out.trim_end_matches('\n');
                    if quoted || f.single {
                        f.push_text(out, quoted);
                    } else {
                        let ifs = self.ifs();
                        f.push_split(out, &ifs);
                    }
                }
                Part::Arith(w) => {
                    let expr = self.expand_str(w)?;
                    let v = self.arith(&expr)?;
                    f.push_text(&v.to_string(), quoted);
                }
            }
        }
        Ok(())
    }

    pub fn arith(&mut self, expr: &str) -> XResult<i64> {
        arith::eval(expr, self).map_err(ExpandError)
    }

    fn base_param(&self, name: &str) -> Option<Value> {
        let s = |v: String| Some(Value::Str(v));
        match name {
            "?" => s(self.status.to_string()),
            "$" => s(self.pid.to_string()),
            "#" => s(self.args.len().to_string()),
            "@" | "*" => Some(Value::Array(self.args.clone())),
            "0" => s(self.arg0.clone()),
            "!" => self.last_bg.map(|j| Value::Str(j.to_string())),
            "-" => s(self.flags()),
            "RANDOM" | "SECONDS" => self.get(name).map(Value::Str),
            n if n.chars().all(|c| c.is_ascii_digit()) => {
                let i: usize = n.parse().ok()?;
                self.args.get(i.checked_sub(1)?).cloned().map(Value::Str)
            }
            n => self.vars.get(n).map(|v| v.value.clone()),
        }
    }

    fn param(&mut self, p: &Param) -> XResult<PRes> {
        let r = self.param_value(p)?;
        let f = &p.flags;
        let PRes::Val(Some(mut v), mut splat) = r else { return Ok(r) };
        if let Some(sep) = &f.split {
            let parts = |s: &str| -> Vec<String> {
                if sep.is_empty() {
                    let ifs = self.ifs();
                    s.split(|c| ifs.contains(c)).filter(|w| !w.is_empty()).map(String::from).collect()
                } else {
                    s.split(sep.as_str()).filter(|w| !w.is_empty()).map(String::from).collect()
                }
            };
            v = Value::Array(match v {
                Value::Str(s) => parts(&s),
                Value::Array(a) => a.iter().flat_map(|s| parts(s)).collect(),
            });
            // `${=x}` and `(s)` split even inside double quotes.
            splat = true;
        }
        if let Some(sep) = &f.join
            && let Value::Array(a) = &v
        {
            v = Value::Str(a.join(sep));
        }
        if f.upper || f.lower {
            let conv = |s: &str| if f.upper { s.to_uppercase() } else { s.to_lowercase() };
            v = match v {
                Value::Str(s) => Value::Str(conv(&s)),
                Value::Array(a) => Value::Array(a.iter().map(|s| conv(s)).collect()),
            };
        }
        Ok(PRes::Val(Some(v), splat || f.splat))
    }

    fn param_value(&mut self, p: &Param) -> XResult<PRes> {
        let mut val = match &p.inner {
            None => self.base_param(&p.name),
            Some(inner) => match &**inner {
                Part::Param(ip) => match self.param(ip)? {
                    PRes::Val(v, _) => v,
                    PRes::Word(w) => Some(Value::Str(self.expand_str(&w)?)),
                },
                part => Some(Value::Str(self.expand_str(&Word(vec![part.clone()]))?.trim_end_matches('\n').to_string())),
            },
        };
        let mut splat = p.name == "@";
        if let Some(idx) = &p.index {
            let idx = self.expand_str(idx)?;
            if idx == "@" || idx == "*" {
                splat = idx == "@";
                if let Some(Value::Str(s)) = val {
                    val = Some(Value::Array(vec![s]));
                }
            } else {
                val = self.subscript(val, &idx)?;
            }
        }
        if p.length {
            let n = match &val {
                None => 0,
                Some(Value::Str(s)) => s.chars().count(),
                Some(Value::Array(a)) => a.len(),
            };
            return Ok(PRes::Val(Some(Value::Str(n.to_string())), false));
        }
        let Some(op) = &p.op else {
            if val.is_none() && p.inner.is_none() && self.opts.nounset && !"@*#?$-!0".contains(p.name.as_str()) {
                return xerr(format!("{}: parameter not set", p.name));
            }
            return Ok(PRes::Val(val, splat));
        };
        let is_null = match &val {
            None => true,
            Some(Value::Str(s)) => s.is_empty(),
            Some(Value::Array(a)) => a.is_empty(),
        };
        let unset_or = |colon: bool| val.is_none() || (colon && is_null);
        let map = |val: Option<Value>, f: &dyn Fn(&str) -> String| -> Option<Value> {
            val.map(|v| match v {
                Value::Str(s) => Value::Str(f(&s)),
                Value::Array(a) => Value::Array(a.iter().map(|s| f(s)).collect()),
            })
        };
        Ok(match op {
            ParamOp::Default { colon, word } => {
                if unset_or(*colon) {
                    PRes::Word(word.clone())
                } else {
                    PRes::Val(val, splat)
                }
            }
            ParamOp::Alt { colon, word } => {
                if unset_or(*colon) {
                    PRes::Val(None, false)
                } else {
                    PRes::Word(word.clone())
                }
            }
            ParamOp::Assign { colon, word } => {
                if unset_or(*colon) {
                    let v = self.expand_str(word)?;
                    if !crate::parser::is_name(&p.name) {
                        return xerr(format!("{}: cannot assign in this way", p.name));
                    }
                    self.set_str(&p.name, v.clone());
                    PRes::Val(Some(Value::Str(v)), false)
                } else {
                    PRes::Val(val, splat)
                }
            }
            ParamOp::Error { colon, word } => {
                if unset_or(*colon) {
                    let msg = self.expand_str(word)?;
                    let msg = if msg.is_empty() { "parameter null or not set".to_string() } else { msg };
                    return xerr(format!("{}: {}", p.name, msg));
                }
                PRes::Val(val, splat)
            }
            ParamOp::TrimPrefix { longest, pat } => {
                let pat = self.expand_pattern(pat)?;
                PRes::Val(map(val, &|s| glob::trim_prefix(s, &pat, *longest)), splat)
            }
            ParamOp::TrimSuffix { longest, pat } => {
                let pat = self.expand_pattern(pat)?;
                PRes::Val(map(val, &|s| glob::trim_suffix(s, &pat, *longest)), splat)
            }
            ParamOp::Replace { all, anchor, pat, rep } => {
                let pat = self.expand_pattern(pat)?;
                let rep = self.expand_str(rep)?;
                PRes::Val(map(val, &|s| glob::replace(s, &pat, &rep, *all, *anchor)), splat)
            }
            ParamOp::Substr { offset, length } => {
                let off = self.expand_str(offset)?;
                let off = self.arith(&off)?;
                let len = match length {
                    Some(l) => {
                        let l = self.expand_str(l)?;
                        Some(self.arith(&l)?)
                    }
                    None => None,
                };
                match val {
                    Some(Value::Array(a)) => PRes::Val(Some(Value::Array(slice(&a, off, len))), splat),
                    Some(Value::Str(s)) => {
                        let chars: Vec<char> = s.chars().collect();
                        PRes::Val(Some(Value::Str(slice(&chars, off, len).into_iter().collect())), false)
                    }
                    None => PRes::Val(None, false),
                }
            }
            ParamOp::Upper => PRes::Val(map(val, &|s| s.to_uppercase()), splat),
            ParamOp::Lower => PRes::Val(map(val, &|s| s.to_lowercase()), splat),
        })
    }

    /// zsh subscripts: 1-based, negative from the end, `a,b` ranges. Strings
    /// are indexed by character.
    fn subscript(&mut self, val: Option<Value>, idx: &str) -> XResult<Option<Value>> {
        let (a, b) = match idx.split_once(',') {
            Some((a, b)) => (self.arith(a)?, Some(self.arith(b)?)),
            None => (self.arith(idx)?, None),
        };
        let pick = |n: usize| -> Option<(usize, usize)> {
            let fix = |i: i64| -> i64 { if i < 0 { n as i64 + i + 1 } else { i } };
            let lo = fix(a).max(1);
            let hi = match b {
                Some(b) => fix(b).min(n as i64),
                None => fix(a),
            };
            if lo > hi || lo > n as i64 || hi < 1 { None } else { Some((lo as usize - 1, hi as usize)) }
        };
        Ok(match val {
            None => None,
            Some(Value::Array(arr)) => match (pick(arr.len()), b) {
                (Some((lo, hi)), Some(_)) => Some(Value::Array(arr[lo..hi].to_vec())),
                (Some((lo, _)), None) => Some(Value::Str(arr[lo].clone())),
                (None, Some(_)) => Some(Value::Array(Vec::new())),
                (None, None) => None,
            },
            Some(Value::Str(s)) => {
                let chars: Vec<char> = s.chars().collect();
                match pick(chars.len()) {
                    Some((lo, hi)) => Some(Value::Str(chars[lo..hi].iter().collect())),
                    None => Some(Value::Str(String::new())),
                }
            }
        })
    }

    /// Tilde expansion at the start of the word (and after `:` in assignments).
    fn tilde(&self, w: &Word, after_colon: bool) -> Word {
        let Some(Part::Lit(first)) = w.0.first() else { return w.clone() };
        if !first.starts_with('~') && !(after_colon && first.contains(":~")) {
            return w.clone();
        }
        let next_is_slash_or_end = w.0.len() == 1;
        let mut out = Vec::new();
        let segments: Vec<&str> = if after_colon { first.split(':').collect() } else { vec![first.as_str()] };
        for (i, seg) in segments.iter().enumerate() {
            if i > 0 {
                out.push(Part::Lit(":".into()));
            }
            if let Some(rest) = seg.strip_prefix('~') {
                let (user, tail) = match rest.find('/') {
                    Some(k) => (&rest[..k], &rest[k..]),
                    None => (rest, ""),
                };
                // `~user` must end the literal or be followed by `/`.
                let ok = !tail.is_empty() || next_is_slash_or_end || i + 1 < segments.len();
                if ok && let Some(home) = self.tilde_dir(user) {
                    out.push(Part::Quoted(home));
                    if !tail.is_empty() {
                        out.push(Part::Lit(tail.to_string()));
                    }
                    continue;
                }
            }
            out.push(Part::Lit(seg.to_string()));
        }
        out.extend(w.0[1..].iter().cloned());
        Word(out)
    }

    fn tilde_dir(&self, user: &str) -> Option<String> {
        match user {
            "" => self.home().map(|h| display_path(&h)),
            "+" => self.get("PWD"),
            "-" => self.get("OLDPWD"),
            u => user_home(u),
        }
    }
}

#[cfg(unix)]
fn user_home(user: &str) -> Option<String> {
    use std::ffi::{CStr, CString};
    let c = CString::new(user).ok()?;
    // getpwnam isn't thread safe; the result is copied right away.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    let dir = unsafe { CStr::from_ptr((*pw).pw_dir) };
    Some(dir.to_string_lossy().into_owned())
}

#[cfg(not(unix))]
fn user_home(user: &str) -> Option<String> {
    let profile = std::env::var("USERPROFILE").ok()?;
    let parent = std::path::Path::new(&profile).parent()?;
    let p = parent.join(user);
    p.is_dir().then(|| display_path(&p))
}

fn slice<T: Clone>(v: &[T], off: i64, len: Option<i64>) -> Vec<T> {
    let n = v.len() as i64;
    let start = if off < 0 { (n + off).max(0) } else { off.min(n) };
    let end = match len {
        None => n,
        Some(l) if l < 0 => (n + l).max(start),
        Some(l) => (start + l).min(n),
    };
    v[start as usize..end as usize].to_vec()
}

impl arith::Env for Shell {
    fn get(&mut self, name: &str) -> Option<String> {
        Shell::get(self, name)
    }
    fn set(&mut self, name: &str, value: i64) {
        self.set_str(name, value.to_string());
    }
}

/// `{a,b}` and `{1..3}` expansion on unquoted literal text.
pub fn brace_expand(w: &Word) -> Vec<Word> {
    for (i, part) in w.0.iter().enumerate() {
        let Part::Lit(s) = part else { continue };
        let Some((open, close, alts)) = find_brace(s) else { continue };
        let mut out = Vec::new();
        for alt in alts {
            let mut parts = w.0[..i].to_vec();
            parts.push(Part::Lit(format!("{}{}{}", &s[..open], alt, &s[close + 1..])));
            parts.extend(w.0[i + 1..].iter().cloned());
            out.extend(brace_expand(&Word(parts)));
        }
        return out;
    }
    vec![w.clone()]
}

fn find_brace(s: &str) -> Option<(usize, usize, Vec<String>)> {
    let b: Vec<(usize, char)> = s.char_indices().collect();
    for (k, &(open, c)) in b.iter().enumerate() {
        if c != '{' {
            continue;
        }
        let mut depth = 0;
        let mut commas = Vec::new();
        let mut close = None;
        for &(j, d) in &b[k..] {
            match d {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(j);
                        break;
                    }
                }
                ',' if depth == 1 => commas.push(j),
                _ => {}
            }
        }
        let Some(close) = close else { continue };
        let inner = &s[open + 1..close];
        if !commas.is_empty() {
            let mut alts = Vec::new();
            let mut start = open + 1;
            for c in commas {
                alts.push(s[start..c].to_string());
                start = c + 1;
            }
            alts.push(s[start..close].to_string());
            return Some((open, close, alts));
        }
        if let Some(seq) = brace_seq(inner) {
            return Some((open, close, seq));
        }
    }
    None
}

fn brace_seq(inner: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = inner.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let step: i64 = if parts.len() == 3 { parts[2].parse().ok()? } else { 1 };
    let step = step.unsigned_abs().max(1) as i64;
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let width = if (parts[0].starts_with('0') && parts[0].len() > 1) || (parts[1].starts_with('0') && parts[1].len() > 1) {
            parts[0].len().max(parts[1].len())
        } else {
            0
        };
        let mut out = Vec::new();
        let mut x = a;
        loop {
            out.push(format!("{x:0width$}"));
            if out.len() > 100_000 {
                return None;
            }
            if a <= b {
                x += step;
                if x > b {
                    break;
                }
            } else {
                x -= step;
                if x < b {
                    break;
                }
            }
        }
        return Some(out);
    }
    let (a, b) = (parts[0], parts[1]);
    if a.chars().count() == 1 && b.chars().count() == 1 {
        let (a, b) = (a.chars().next()? as u32, b.chars().next()? as u32);
        let range: Vec<u32> = if a <= b { (a..=b).step_by(step as usize).collect() } else { (b..=a).rev().step_by(step as usize).collect() };
        return Some(range.into_iter().filter_map(char::from_u32).map(String::from).collect());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &str) -> Vec<String> {
        brace_expand(&Word::lit(w)).into_iter().map(|w| w.as_plain().unwrap()).collect()
    }

    #[test]
    fn braces() {
        assert_eq!(words("a{b,c}d"), ["abd", "acd"]);
        assert_eq!(words("{1..3}"), ["1", "2", "3"]);
        assert_eq!(words("{3..1}"), ["3", "2", "1"]);
        assert_eq!(words("{01..03}"), ["01", "02", "03"]);
        assert_eq!(words("{a..c}"), ["a", "b", "c"]);
        assert_eq!(words("{1..10..4}"), ["1", "5", "9"]);
        assert_eq!(words("x{a,{b,c}}"), ["xa", "xb", "xc"]);
        assert_eq!(words("{a,b}{1,2}"), ["a1", "a2", "b1", "b2"]);
        assert_eq!(words("{}"), ["{}"]);
        assert_eq!(words("{x}"), ["{x}"]);
    }
}
