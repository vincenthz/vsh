//! Tiny getopt for the builtin utilities.
//!
//! `-abc` bundles flags, `-n5` / `-n 5` give values to the letters listed in
//! `with_value`, `--long` and `--long=value` are kept by name, `--` ends
//! options. A lone `-` is an operand (stdin). Numeric shorthands such as
//! `head -5` are left to the caller via `Opts::number`.

#[derive(Debug, Default)]
pub struct Opts {
    flags: Vec<char>,
    values: Vec<(char, String)>,
    longs: Vec<(String, Option<String>)>,
    /// A leading `-NUM` option (as in `head -5`), if `allow_number` was set.
    pub number: Option<String>,
    pub operands: Vec<String>,
}

pub struct Spec<'a> {
    /// Short letters that take a value.
    pub with_value: &'a str,
    /// Long options that take a value (`--name value` or `--name=value`).
    pub long_with_value: &'a [&'a str],
    /// Accept `-123` as `number`.
    pub allow_number: bool,
    /// Stop at the first operand (like `env` or `find`).
    pub stop_at_operand: bool,
}

pub const SIMPLE: Spec<'static> = Spec { with_value: "", long_with_value: &[], allow_number: false, stop_at_operand: false };

pub fn parse(args: &[String], spec: &Spec) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if a == "--" {
            o.operands.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, val) = match long.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (long.to_string(), None),
            };
            let val = if val.is_none() && spec.long_with_value.contains(&name.as_str()) {
                if i >= args.len() {
                    return Err(format!("option '--{name}' requires an argument"));
                }
                i += 1;
                Some(args[i - 1].clone())
            } else {
                val
            };
            o.longs.push((name, val));
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let body = &a[1..];
            if spec.allow_number && body.chars().all(|c| c.is_ascii_digit()) {
                o.number = Some(body.to_string());
                continue;
            }
            let chars: Vec<char> = body.chars().collect();
            let mut j = 0;
            while j < chars.len() {
                let c = chars[j];
                j += 1;
                if spec.with_value.contains(c) {
                    let rest: String = chars[j..].iter().collect();
                    let v = if !rest.is_empty() {
                        rest
                    } else if i < args.len() {
                        i += 1;
                        args[i - 1].clone()
                    } else {
                        return Err(format!("option requires an argument -- '{c}'"));
                    };
                    o.values.push((c, v));
                    break;
                }
                o.flags.push(c);
            }
            continue;
        }
        o.operands.push(a.clone());
        if spec.stop_at_operand {
            o.operands.extend(args[i..].iter().cloned());
            break;
        }
    }
    Ok(o)
}

impl Opts {
    pub fn flag(&self, c: char) -> bool {
        self.flags.contains(&c)
    }

    /// Last value given to a short option.
    pub fn value(&self, c: char) -> Option<&str> {
        self.values.iter().rev().find(|(k, _)| *k == c).map(|(_, v)| v.as_str())
    }

    pub fn values(&self, c: char) -> impl Iterator<Item = &str> {
        self.values.iter().filter(move |(k, _)| *k == c).map(|(_, v)| v.as_str())
    }

    pub fn long(&self, name: &str) -> bool {
        self.longs.iter().any(|(n, _)| n == name)
    }

    pub fn long_value(&self, name: &str) -> Option<&str> {
        self.longs.iter().rev().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }

    /// Short flags and long options that are not in the allowed sets.
    pub fn unknown(&self, shorts: &str, longs: &[&str]) -> Option<String> {
        for &c in &self.flags {
            if !shorts.contains(c) {
                return Some(format!("invalid option -- '{c}'"));
            }
        }
        for (c, _) in &self.values {
            if !shorts.contains(*c) {
                return Some(format!("invalid option -- '{c}'"));
            }
        }
        for (n, _) in &self.longs {
            if !longs.contains(&n.as_str()) {
                return Some(format!("unrecognized option '--{n}'"));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn bundles_and_values() {
        let spec = Spec { with_value: "n", long_with_value: &["lines"], allow_number: true, stop_at_operand: false };
        let o = parse(&s(&["-la", "-n3", "x", "--lines", "4", "--color=auto", "-", "-7", "--", "-q"]), &spec).unwrap();
        assert!(o.flag('l') && o.flag('a'));
        assert_eq!(o.value('n'), Some("3"));
        assert_eq!(o.long_value("lines"), Some("4"));
        assert_eq!(o.long_value("color"), Some("auto"));
        assert_eq!(o.number.as_deref(), Some("7"));
        assert_eq!(o.operands, s(&["x", "-", "-q"]));
    }
}
