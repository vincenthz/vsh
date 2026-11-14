//! Integer arithmetic for `$(( ))`, `(( ))`, `let` and array subscripts.

pub trait Env {
    fn get(&mut self, name: &str) -> Option<String>;
    fn set(&mut self, name: &str, value: i64);
}

pub fn eval(expr: &str, env: &mut dyn Env) -> Result<i64, String> {
    let toks = lex(expr)?;
    if toks.is_empty() {
        return Ok(0);
    }
    let mut p = P { toks, pos: 0, env, depth: 0, skip: 0 };
    let v = p.comma()?;
    if p.pos < p.toks.len() {
        return Err(format!("syntax error in expression: {expr}"));
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq)]
enum T {
    Num(i64),
    Ident(String),
    Op(&'static str),
}

const OPS: &[&str] = &[
    "<<=", ">>=", "**=", "**", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "++", "--", "+=", "-=", "*=", "/=", "%=",
    "&=", "^=", "|=", "+", "-", "*", "/", "%", "<", ">", "=", "!", "~", "&", "^", "|", "?", ":", "(", ")", ",",
];

fn lex(s: &str) -> Result<Vec<T>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    'outer: while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '#' || chars[i] == '_') {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            out.push(T::Num(parse_num(&text)?));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(T::Ident(chars[start..i].iter().collect()));
            continue;
        }
        let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
        for op in OPS {
            if rest.starts_with(op) {
                out.push(T::Op(op));
                i += op.len();
                continue 'outer;
            }
        }
        return Err(format!("bad math expression: illegal character: {c}"));
    }
    Ok(out)
}

pub fn parse_num(text: &str) -> Result<i64, String> {
    let bad = || format!("bad math expression: {text}");
    let t = text.replace('_', "");
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(h, 16).map_err(|_| bad());
    }
    if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        return i64::from_str_radix(b, 2).map_err(|_| bad());
    }
    if let Some((base, digits)) = t.split_once('#') {
        let base: u32 = base.parse().map_err(|_| bad())?;
        if !(2..=36).contains(&base) {
            return Err(bad());
        }
        return i64::from_str_radix(digits, base).map_err(|_| bad());
    }
    t.parse().map_err(|_| bad())
}

struct P<'a> {
    toks: Vec<T>,
    pos: usize,
    env: &'a mut dyn Env,
    depth: usize,
    /// >0 while evaluating the untaken side of `&&`, `||`, `?:` (no side effects).
    skip: usize,
}

impl P<'_> {
    fn peek_op(&self) -> Option<&'static str> {
        match self.toks.get(self.pos) {
            Some(T::Op(o)) => Some(o),
            _ => None,
        }
    }

    fn eat(&mut self, op: &str) -> bool {
        if self.peek_op() == Some(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn var(&mut self, name: &str) -> Result<i64, String> {
        let v = self.env.get(name).unwrap_or_default();
        let v = v.trim();
        if v.is_empty() {
            return Ok(0);
        }
        if let Ok(n) = parse_num(v) {
            return Ok(n);
        }
        if self.depth > 32 {
            return Err(format!("{name}: expression recursion too deep"));
        }
        self.depth += 1;
        let r = eval_nested(v, self.env, self.depth);
        self.depth -= 1;
        r
    }

    fn set(&mut self, name: &str, v: i64) {
        if self.skip == 0 {
            self.env.set(name, v);
        }
    }

    fn comma(&mut self) -> Result<i64, String> {
        let mut v = self.assign()?;
        while self.eat(",") {
            v = self.assign()?;
        }
        Ok(v)
    }

    fn assign(&mut self) -> Result<i64, String> {
        if let (Some(T::Ident(name)), Some(T::Op(op))) = (self.toks.get(self.pos).cloned(), self.toks.get(self.pos + 1).cloned())
            && matches!(op, "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|=" | "**=")
        {
            self.pos += 2;
            let rhs = self.assign()?;
            let v = if op == "=" {
                rhs
            } else {
                let cur = self.var(&name)?;
                binop(&op[..op.len() - 1], cur, rhs)?
            };
            self.set(&name, v);
            return Ok(v);
        }
        self.ternary()
    }

    fn ternary(&mut self) -> Result<i64, String> {
        let c = self.binary(0)?;
        if self.eat("?") {
            if c == 0 {
                self.skip += 1;
            }
            let a = self.assign();
            if c == 0 {
                self.skip -= 1;
            }
            if !self.eat(":") {
                return Err("':' expected".into());
            }
            if c != 0 {
                self.skip += 1;
            }
            let b = self.assign();
            if c != 0 {
                self.skip -= 1;
            }
            return if c != 0 { a } else { b };
        }
        Ok(c)
    }

    fn binary(&mut self, min_prec: u8) -> Result<i64, String> {
        let mut lhs = self.unary()?;
        while let Some(op) = self.peek_op() {
            let Some(prec) = prec(op) else { break };
            if prec < min_prec {
                break;
            }
            self.pos += 1;
            if op == "&&" || op == "||" {
                let short = if op == "&&" { lhs == 0 } else { lhs != 0 };
                if short {
                    self.skip += 1;
                }
                let rhs = self.binary(prec + 1);
                if short {
                    self.skip -= 1;
                }
                let rhs = rhs?;
                lhs = if short { (op == "||") as i64 } else { (rhs != 0) as i64 };
                continue;
            }
            // `**` is right associative.
            let next = if op == "**" { prec } else { prec + 1 };
            let rhs = self.binary(next)?;
            lhs = if self.skip > 0 { 0 } else { binop(op, lhs, rhs)? };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<i64, String> {
        if let Some(op) = self.peek_op() {
            match op {
                "-" | "+" | "!" | "~" => {
                    self.pos += 1;
                    let v = self.unary()?;
                    return Ok(match op {
                        "-" => v.wrapping_neg(),
                        "+" => v,
                        "!" => (v == 0) as i64,
                        _ => !v,
                    });
                }
                "++" | "--" => {
                    self.pos += 1;
                    let Some(T::Ident(name)) = self.toks.get(self.pos).cloned() else {
                        return Err("bad math expression: lvalue required".into());
                    };
                    self.pos += 1;
                    let v = self.var(&name)? + if op == "++" { 1 } else { -1 };
                    self.set(&name, v);
                    return Ok(v);
                }
                _ => {}
            }
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<i64, String> {
        match self.toks.get(self.pos).cloned() {
            Some(T::Num(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(T::Ident(name)) => {
                self.pos += 1;
                let v = self.var(&name)?;
                if let Some(op @ ("++" | "--")) = self.peek_op() {
                    self.pos += 1;
                    self.set(&name, v + if op == "++" { 1 } else { -1 });
                }
                Ok(v)
            }
            Some(T::Op("(")) => {
                self.pos += 1;
                let v = self.comma()?;
                if !self.eat(")") {
                    return Err("')' expected".into());
                }
                Ok(v)
            }
            _ => Err("bad math expression: operand expected".into()),
        }
    }
}

fn eval_nested(expr: &str, env: &mut dyn Env, depth: usize) -> Result<i64, String> {
    let toks = lex(expr)?;
    let mut p = P { toks, pos: 0, env, depth, skip: 0 };
    let v = p.comma()?;
    Ok(v)
}

fn prec(op: &str) -> Option<u8> {
    Some(match op {
        "||" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" => 6,
        "<" | "<=" | ">" | ">=" => 7,
        "<<" | ">>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        "**" => 11,
        _ => return None,
    })
}

fn binop(op: &str, a: i64, b: i64) -> Result<i64, String> {
    Ok(match op {
        "+" => a.wrapping_add(b),
        "-" => a.wrapping_sub(b),
        "*" => a.wrapping_mul(b),
        "/" | "%" if b == 0 => return Err("division by zero".into()),
        "/" => a.wrapping_div(b),
        "%" => a.wrapping_rem(b),
        "**" => {
            if b < 0 {
                0
            } else {
                a.wrapping_pow(b.min(u32::MAX as i64) as u32)
            }
        }
        "<<" => a.wrapping_shl(b as u32),
        ">>" => a.wrapping_shr(b as u32),
        "<" => (a < b) as i64,
        "<=" => (a <= b) as i64,
        ">" => (a > b) as i64,
        ">=" => (a >= b) as i64,
        "==" => (a == b) as i64,
        "!=" => (a != b) as i64,
        "&" => a & b,
        "^" => a ^ b,
        "|" => a | b,
        _ => return Err(format!("bad operator {op}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct M(HashMap<String, String>);
    impl Env for M {
        fn get(&mut self, n: &str) -> Option<String> {
            self.0.get(n).cloned()
        }
        fn set(&mut self, n: &str, v: i64) {
            self.0.insert(n.into(), v.to_string());
        }
    }

    fn ev(s: &str, m: &mut M) -> i64 {
        eval(s, m).unwrap()
    }

    #[test]
    fn arithmetic() {
        let mut m = M(HashMap::new());
        assert_eq!(ev("1 + 2 * 3", &mut m), 7);
        assert_eq!(ev("(1 + 2) * 3", &mut m), 9);
        assert_eq!(ev("2 ** 3 ** 2", &mut m), 512);
        assert_eq!(ev("-3 % 2", &mut m), -1);
        assert_eq!(ev("0x10 + 8#17 + 2#101", &mut m), 16 + 15 + 5);
        assert_eq!(ev("x = 5, x += 2, x", &mut m), 7);
        assert_eq!(ev("x++ + ++x", &mut m), 7 + 9);
        assert_eq!(ev("y ? 1 : 2", &mut m), 2);
        assert_eq!(ev("0 && (z = 1)", &mut m), 0);
        assert_eq!(m.0.get("z"), None);
        assert_eq!(ev("1 < 2 && 3 >= 3", &mut m), 1);
        m.0.insert("e".into(), "x * 2".into());
        assert_eq!(ev("e + 1", &mut m), 19);
        assert!(eval("1 / 0", &mut m).is_err());
        assert!(eval("1 +", &mut m).is_err());
    }
}
