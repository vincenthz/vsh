//! Turning syntax trees and values back into shell text (for `type`,
//! `functions`, `jobs`, xtrace, `alias`, `set`).

use crate::ast::*;

/// Quote a string so the shell reads it back as one word.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// An io::Error in zsh's lowercase style, without " (os error N)".
pub fn io_error(e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => "no such file or directory".into(),
        PermissionDenied => "permission denied".into(),
        IsADirectory => "is a directory".into(),
        NotADirectory => "not a directory".into(),
        AlreadyExists => "file exists".into(),
        DirectoryNotEmpty => "directory not empty".into(),
        _ => {
            let s = e.to_string();
            let s = match s.find(" (os error") {
                Some(i) => s[..i].to_string(),
                None => s,
            };
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_lowercase().chain(c).collect(),
                None => s,
            }
        }
    }
}

pub fn list(l: &List, indent: usize) -> String {
    let pad = "    ".repeat(indent);
    l.iter()
        .map(|item| format!("{pad}{}{}", and_or(&item.and_or), if item.background { " &" } else { "" }))
        .collect::<Vec<_>>()
        .join("\n")
}

fn inline(l: &List) -> String {
    l.iter()
        .map(|item| format!("{}{}", and_or(&item.and_or), if item.background { " &" } else { ";" }))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn and_or(a: &AndOr) -> String {
    let mut s = pipeline(&a.first);
    for (c, p) in &a.rest {
        s.push_str(if *c == Connector::And { " && " } else { " || " });
        s.push_str(&pipeline(p));
    }
    s
}

fn pipeline(p: &Pipeline) -> String {
    let mut s = String::new();
    if p.negate {
        s.push_str("! ");
    }
    for (i, c) in p.cmds.iter().enumerate() {
        if i > 0 {
            s.push_str(if p.stderr_too[i - 1] { " |& " } else { " | " });
        }
        s.push_str(&command(c, 0));
    }
    s
}

pub fn command(c: &Command, indent: usize) -> String {
    match c {
        Command::Simple(s) => {
            let mut parts: Vec<String> = Vec::new();
            for a in &s.assigns {
                let v = match &a.value {
                    AssignValue::Scalar(w) => word(w),
                    AssignValue::Array(ws) => format!("({})", ws.iter().map(word).collect::<Vec<_>>().join(" ")),
                };
                parts.push(format!("{}{}={}", a.name, if a.append { "+" } else { "" }, v));
            }
            parts.extend(s.words.iter().map(word));
            parts.extend(s.redirs.iter().map(redir));
            parts.join(" ")
        }
        Command::Compound(c, redirs) => {
            let mut s = compound(c, indent);
            for r in redirs {
                s.push(' ');
                s.push_str(&redir(r));
            }
            s
        }
        Command::FuncDef(name, body) => format!("{name} () {}", command(body, indent)),
    }
}

fn block(l: &List, indent: usize) -> String {
    format!("\n{}\n{}", list(l, indent + 1), "    ".repeat(indent))
}

fn compound(c: &Compound, indent: usize) -> String {
    match c {
        Compound::Brace(l) => format!("{{{}}}", block(l, indent)),
        Compound::Subshell(l) => format!("( {} )", inline(l)),
        Compound::If { arms, otherwise } => {
            let mut s = String::new();
            for (i, (c, b)) in arms.iter().enumerate() {
                s.push_str(if i == 0 { "if " } else { "elif " });
                s.push_str(&format!("{}\n{}then{}", inline(c), "    ".repeat(indent), block(b, indent)));
            }
            if let Some(o) = otherwise {
                s.push_str(&format!("else{}", block(o, indent)));
            }
            s.push_str("fi");
            s
        }
        Compound::While { cond, body, until } => {
            format!("{} {}\n{}do{}done", if *until { "until" } else { "while" }, inline(cond), "    ".repeat(indent), block(body, indent))
        }
        Compound::For { var, items, body } => {
            let items = match items {
                Some(ws) => format!(" in {}", ws.iter().map(word).collect::<Vec<_>>().join(" ")),
                None => String::new(),
            };
            format!("for {var}{items}\n{}do{}done", "    ".repeat(indent), block(body, indent))
        }
        Compound::ForArith { init, cond, step, body } => {
            format!("for (({init}; {cond}; {step}))\n{}do{}done", "    ".repeat(indent), block(body, indent))
        }
        Compound::Repeat { count, body } => format!("repeat {}\n{}do{}done", word(count), "    ".repeat(indent), block(body, indent)),
        Compound::Case { word: w, arms } => {
            let pad = "    ".repeat(indent + 1);
            let mut s = format!("case {} in\n", word(w));
            for a in arms {
                let pats: Vec<String> = a.patterns.iter().map(word).collect();
                s.push_str(&format!("{pad}({})\n{}\n{pad};{}\n", pats.join(" | "), list(&a.body, indent + 2), if a.fallthrough { "&" } else { ";" }));
            }
            s.push_str(&format!("{}esac", "    ".repeat(indent)));
            s
        }
        Compound::Cond(toks) => {
            let parts: Vec<String> = toks
                .iter()
                .map(|t| match t {
                    CondTok::Word(w) => word(w),
                    CondTok::Op(o) => o.clone(),
                })
                .collect();
            format!("[[ {} ]]", parts.join(" "))
        }
        Compound::Arith(e) => format!("(({e}))"),
    }
}

fn redir(r: &Redir) -> String {
    let fd = r.fd.map(|f| f.to_string()).unwrap_or_default();
    let op = match r.op {
        RedirOp::In => "<",
        RedirOp::Out => ">",
        RedirOp::Append => ">>",
        RedirOp::ReadWrite => "<>",
        RedirOp::DupOut => ">&",
        RedirOp::DupIn => "<&",
        RedirOp::OutErr => "&>",
        RedirOp::AppendErr => "&>>",
        RedirOp::HereDoc => "<<",
        RedirOp::HereString => "<<<",
    };
    if r.op == RedirOp::HereDoc {
        return format!("{fd}<<'EOF'\n{}EOF", word_raw(&r.target));
    }
    format!("{fd}{op}{}", word(&r.target))
}

fn word_raw(w: &Word) -> String {
    w.0.iter()
        .map(|p| match p {
            Part::Lit(s) | Part::Quoted(s) => s.clone(),
            other => part(other),
        })
        .collect()
}

pub fn word(w: &Word) -> String {
    w.0.iter().map(part).collect()
}

fn part(p: &Part) -> String {
    match p {
        Part::Lit(s) => s.clone(),
        Part::Quoted(s) => quote_always(s),
        Part::Double(inner) => {
            let body: String = inner
                .iter()
                .map(|p| match p {
                    Part::Quoted(s) => s.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`"),
                    other => part(other),
                })
                .collect();
            format!("\"{body}\"")
        }
        Part::Param(p) => param(p),
        Part::CmdSub(l) => format!("$({})", inline(l).trim_end_matches(';')),
        Part::Arith(w) => format!("$(({}))", word_raw(w)),
    }
}

fn quote_always(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn param(p: &Param) -> String {
    let idx = p.index.as_ref().map(|w| format!("[{}]", word(w))).unwrap_or_default();
    let f = &p.flags;
    let mut flags = String::new();
    if f.upper {
        flags.push('U');
    }
    if f.lower {
        flags.push('L');
    }
    if f.splat {
        flags.push('@');
    }
    if let Some(s) = &f.split {
        if s.is_empty() { flags.push('=') } else { flags.push_str(&format!("s\u{1f}{s}\u{1f}")) }
    }
    if let Some(j) = &f.join {
        flags.push_str(&format!("j\u{1f}{j}\u{1f}"));
    }
    let flags = if flags.is_empty() {
        String::new()
    } else if flags == "=" {
        "=".to_string()
    } else {
        let sep = if flags.contains(':') { '/' } else { ':' };
        format!("({})", flags.replace('\u{1f}', &sep.to_string()))
    };
    let simple = p.op.is_none() && !p.length && flags.is_empty();
    if simple && idx.is_empty() {
        return format!("${}", p.name);
    }
    let len = if p.length { "#" } else { "" };
    let op = match &p.op {
        None => String::new(),
        Some(op) => match op {
            ParamOp::Default { colon, word: w } => format!("{}-{}", if *colon { ":" } else { "" }, word(w)),
            ParamOp::Assign { colon, word: w } => format!("{}={}", if *colon { ":" } else { "" }, word(w)),
            ParamOp::Error { colon, word: w } => format!("{}?{}", if *colon { ":" } else { "" }, word(w)),
            ParamOp::Alt { colon, word: w } => format!("{}+{}", if *colon { ":" } else { "" }, word(w)),
            ParamOp::TrimPrefix { longest, pat } => format!("{}{}", if *longest { "##" } else { "#" }, word(pat)),
            ParamOp::TrimSuffix { longest, pat } => format!("{}{}", if *longest { "%%" } else { "%" }, word(pat)),
            ParamOp::Replace { all, anchor, pat, rep } => format!(
                "/{}{}{}/{}",
                if *all { "/" } else { "" },
                anchor.map(String::from).unwrap_or_default(),
                word(pat),
                word(rep)
            ),
            ParamOp::Substr { offset, length } => {
                format!(":{}{}", word(offset), length.as_ref().map(|l| format!(":{}", word(l))).unwrap_or_default())
            }
            ParamOp::Upper => ":u".into(),
            ParamOp::Lower => ":l".into(),
        },
    };
    let name = match &p.inner {
        Some(inner) => part(inner),
        None => p.name.clone(),
    };
    format!("${{{flags}{len}{name}{idx}{op}}}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;

    #[test]
    fn roundtrip() {
        for src in ["echo a 'b c' \"$x\" ${y:-z} $(ls) > out", "a && b || ! c | d", "f () {\n    echo hi\n}"] {
            let l = Parser::parse_all(src).unwrap();
            let text = list(&l, 0);
            let again = list(&Parser::parse_all(&text).unwrap(), 0);
            assert_eq!(text, again, "{src}");
        }
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("plain/path.txt"), "plain/path.txt");
    }
}
