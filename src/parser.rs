//! Recursive-descent parser for the zsh-flavoured shell language.
//!
//! Lexing and parsing are combined because shell tokenization depends on
//! context (keywords only at command position, `[[ ]]` operators, here-docs).
//! The parser yields one complete command at a time so that aliases and
//! functions defined by a command apply to the following ones.

use std::collections::HashMap;
use std::sync::Arc;

use crate::ast::*;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub msg: String,
    /// The input ended in the middle of a construct (more lines may fix it).
    pub incomplete: bool,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

type PResult<T> = Result<T, ParseError>;

const KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "in", "case", "esac", "repeat",
    "function", "{", "}", "[[", "]]", "!",
];

/// Words that end a list when they appear at command position.
const TERMINATORS: &[&str] = &["then", "elif", "else", "fi", "do", "done", "esac", "}"];

pub struct Parser {
    src: Vec<char>,
    pos: usize,
    aliases: HashMap<String, String>,
    /// Aliases being expanded, with the end of their expansion in `src`.
    alias_guard: Vec<(String, usize)>,
}

fn is_meta(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')')
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub fn is_name(s: &str) -> bool {
    let mut it = s.chars();
    it.next().is_some_and(is_name_start) && it.all(is_name_char)
}

fn err<T>(msg: impl Into<String>) -> PResult<T> {
    Err(ParseError { msg: msg.into(), incomplete: false })
}

fn incomplete<T>(msg: impl Into<String>) -> PResult<T> {
    Err(ParseError { msg: msg.into(), incomplete: true })
}

impl Parser {
    pub fn new(src: &str) -> Parser {
        let src = src.replace("\r\n", "\n");
        Parser { src: src.chars().collect(), pos: 0, aliases: HashMap::new(), alias_guard: Vec::new() }
    }

    /// Parse the next complete command (one logical line). `None` at end of input.
    pub fn next_command(&mut self, aliases: &HashMap<String, String>) -> PResult<Option<List>> {
        self.aliases.clone_from(aliases);
        self.skip_linebreaks();
        if self.at_end() {
            return Ok(None);
        }
        let list = self.parse_list(true)?;
        self.skip_blanks();
        match self.peek() {
            None => {}
            Some('\n') => self.pos += 1,
            Some(_) => return self.unexpected(),
        }
        Ok(Some(list))
    }

    /// Parse the whole input.
    pub fn parse_all(src: &str) -> PResult<List> {
        let mut p = Parser::new(src);
        let mut all = Vec::new();
        while let Some(l) = p.next_command(&HashMap::new())? {
            all.extend(l);
        }
        Ok(all)
    }

    // ---- character helpers ----

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.src.get(self.pos + off).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(k, c)| self.src.get(self.pos + k) == Some(&c))
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.starts_with(s) {
            self.pos += s.chars().count();
            true
        } else {
            false
        }
    }

    /// Skip spaces, tabs, comments and backslash-newline continuations.
    fn skip_blanks(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r') => self.pos += 1,
                Some('\\') if self.peek_at(1) == Some('\n') => self.pos += 2,
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn skip_linebreaks(&mut self) {
        loop {
            self.skip_blanks();
            if self.peek() == Some('\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn unexpected<T>(&self) -> PResult<T> {
        match self.peek() {
            None => incomplete("unexpected end of input"),
            Some('\n') => err("parse error near `\\n'"),
            Some(_) => {
                let tok: String = self.src[self.pos..].iter().take_while(|c| !c.is_whitespace()).take(10).collect();
                err(format!("parse error near `{tok}'"))
            }
        }
    }

    /// The keyword at the current position (after blanks), without consuming it.
    fn peek_keyword(&mut self) -> Option<&'static str> {
        self.skip_blanks();
        for kw in KEYWORDS {
            if self.starts_with(kw) {
                let after = self.peek_at(kw.chars().count());
                if after.is_none_or(|c| is_meta(c) || (*kw == "{" || *kw == "}") && c == '}') {
                    return Some(kw);
                }
            }
        }
        None
    }

    fn expect_keyword(&mut self, kw: &str) -> PResult<()> {
        self.skip_linebreaks();
        if self.peek_keyword() == Some(kw_static(kw)) {
            self.pos += kw.chars().count();
            Ok(())
        } else if self.at_end() {
            incomplete(format!("expected `{kw}'"))
        } else {
            self.unexpected()
        }
    }

    // ---- grammar ----

    /// Items separated by `;`, `&` and (unless `top`) newlines, up to a
    /// terminator keyword, `)`, `;;` or end of input.
    fn parse_list(&mut self, top: bool) -> PResult<List> {
        let mut items = Vec::new();
        loop {
            if top {
                self.skip_blanks();
            } else {
                self.skip_linebreaks();
            }
            if self.at_end() || self.at_list_end() {
                break;
            }
            if top && self.peek() == Some('\n') {
                break;
            }
            let Some(and_or) = self.parse_and_or()? else { break };
            self.skip_blanks();
            let mut background = false;
            if self.peek() == Some('&') && !self.starts_with("&&") {
                self.pos += 1;
                background = true;
            } else if self.peek() == Some(';') && !matches!(self.peek_at(1), Some(';' | '&' | '|')) {
                self.pos += 1;
            } else if self.peek() == Some('\n') {
                if !top {
                    self.pos += 1;
                }
            } else {
                items.push(Item { and_or, background });
                break;
            }
            items.push(Item { and_or, background });
        }
        Ok(items)
    }

    fn at_list_end(&mut self) -> bool {
        if matches!(self.peek(), Some(')')) || self.starts_with(";;") || self.starts_with(";&") || self.starts_with(";|") {
            return true;
        }
        matches!(self.peek_keyword(), Some(kw) if TERMINATORS.contains(&kw))
    }

    fn parse_and_or(&mut self) -> PResult<Option<AndOr>> {
        let Some(first) = self.parse_pipeline()? else { return Ok(None) };
        let mut rest = Vec::new();
        loop {
            self.skip_blanks();
            let conn = if self.eat("&&") {
                Connector::And
            } else if self.eat("||") {
                Connector::Or
            } else {
                break;
            };
            self.skip_linebreaks();
            match self.parse_pipeline()? {
                Some(p) => rest.push((conn, p)),
                None => return self.unexpected(),
            }
        }
        Ok(Some(AndOr { first, rest }))
    }

    fn parse_pipeline(&mut self) -> PResult<Option<Pipeline>> {
        let mut negate = false;
        if self.peek_keyword() == Some("!") {
            self.pos += 1;
            negate = true;
        }
        let Some(first) = self.parse_command()? else {
            return if negate { self.unexpected() } else { Ok(None) };
        };
        let mut cmds = vec![first];
        let mut stderr_too = Vec::new();
        loop {
            self.skip_blanks();
            if self.peek() == Some('|') && self.peek_at(1) != Some('|') {
                self.pos += 1;
                stderr_too.push(self.eat("&"));
                self.skip_linebreaks();
                match self.parse_command()? {
                    Some(c) => cmds.push(c),
                    None => return self.unexpected(),
                }
            } else {
                break;
            }
        }
        Ok(Some(Pipeline { negate, cmds, stderr_too }))
    }

    fn parse_command(&mut self) -> PResult<Option<Command>> {
        self.skip_blanks();
        if self.at_end() {
            return Ok(None);
        }
        let compound = match self.peek_keyword() {
            Some(kw) if TERMINATORS.contains(&kw) || kw == "]]" || kw == "in" => return Ok(None),
            Some("if") => {
                self.pos += 2;
                Some(self.parse_if()?)
            }
            Some(kw @ ("while" | "until")) => {
                self.pos += kw.len();
                let cond = self.parse_list(false)?;
                let body = self.parse_do_group()?;
                Some(Compound::While { cond, body, until: kw == "until" })
            }
            Some("for") => {
                self.pos += 3;
                Some(self.parse_for()?)
            }
            Some("case") => {
                self.pos += 4;
                Some(self.parse_case()?)
            }
            Some("repeat") => {
                self.pos += 6;
                self.skip_blanks();
                let Some(count) = self.read_word()? else { return self.unexpected() };
                self.skip_blanks();
                if self.eat(";") || self.peek() == Some('\n') {
                    self.skip_linebreaks();
                }
                let body = if self.peek_keyword() == Some("do") {
                    self.parse_do_group()?
                } else {
                    match self.parse_command()? {
                        Some(c) => vec![simple_item(c)],
                        None => return self.unexpected(),
                    }
                };
                Some(Compound::Repeat { count, body })
            }
            Some("function") => {
                self.pos += 8;
                self.skip_blanks();
                let Some(name) = self.read_word()?.and_then(|w| w.as_plain()) else { return self.unexpected() };
                self.skip_blanks();
                if self.eat("(") {
                    self.skip_blanks();
                    if !self.eat(")") {
                        return self.unexpected();
                    }
                }
                return self.parse_func_body(name).map(Some);
            }
            Some("{") => {
                self.pos += 1;
                let body = self.parse_list(false)?;
                self.expect_keyword("}")?;
                Some(Compound::Brace(body))
            }
            Some("[[") => {
                self.pos += 2;
                Some(self.parse_cond()?)
            }
            _ => None,
        };
        let compound = match compound {
            Some(c) => c,
            None if self.starts_with("((") => {
                self.pos += 2;
                let expr = self.read_arith_raw()?;
                Compound::Arith(expr)
            }
            None if self.peek() == Some('(') => {
                self.pos += 1;
                let body = self.parse_list(false)?;
                self.skip_linebreaks();
                if !self.eat(")") {
                    return if self.at_end() { incomplete("expected `)'") } else { self.unexpected() };
                }
                Compound::Subshell(body)
            }
            None => return self.parse_simple(),
        };
        let mut redirs = Vec::new();
        loop {
            self.skip_blanks();
            match self.try_redir()? {
                Some(r) => redirs.push(r),
                None => break,
            }
        }
        Ok(Some(Command::Compound(Box::new(compound), redirs)))
    }

    fn parse_func_body(&mut self, name: String) -> PResult<Command> {
        self.skip_linebreaks();
        match self.parse_command()? {
            Some(body) => Ok(Command::FuncDef(name, Arc::new(body))),
            None => self.unexpected(),
        }
    }

    fn parse_do_group(&mut self) -> PResult<List> {
        self.expect_keyword("do")?;
        let body = self.parse_list(false)?;
        self.expect_keyword("done")?;
        Ok(body)
    }

    fn parse_if(&mut self) -> PResult<Compound> {
        let mut arms = Vec::new();
        let mut otherwise = None;
        loop {
            let cond = self.parse_list(false)?;
            self.expect_keyword("then")?;
            let body = self.parse_list(false)?;
            arms.push((cond, body));
            self.skip_linebreaks();
            match self.peek_keyword() {
                Some("elif") => self.pos += 4,
                Some("else") => {
                    self.pos += 4;
                    otherwise = Some(self.parse_list(false)?);
                    self.expect_keyword("fi")?;
                    break;
                }
                Some("fi") => {
                    self.pos += 2;
                    break;
                }
                _ if self.at_end() => return incomplete("expected `fi'"),
                _ => return self.unexpected(),
            }
        }
        Ok(Compound::If { arms, otherwise })
    }

    fn parse_for(&mut self) -> PResult<Compound> {
        self.skip_blanks();
        if self.eat("((") {
            let raw = self.read_arith_raw()?;
            let parts: Vec<&str> = raw.splitn(3, ';').collect();
            if parts.len() != 3 {
                return err("parse error in for ((...))");
            }
            self.skip_blanks();
            self.eat(";");
            self.skip_linebreaks();
            let body = self.parse_loop_body()?;
            return Ok(Compound::ForArith {
                init: parts[0].trim().into(),
                cond: parts[1].trim().into(),
                step: parts[2].trim().into(),
                body,
            });
        }
        let Some(var) = self.read_word()?.and_then(|w| w.as_plain()).filter(|v| is_name(v)) else {
            return err("parse error: bad for variable");
        };
        self.skip_blanks();
        let mut items = None;
        if self.peek() == Some('(') {
            // zsh: for x (a b c) cmd
            self.pos += 1;
            let mut words = Vec::new();
            loop {
                self.skip_linebreaks();
                if self.eat(")") {
                    break;
                }
                match self.read_word()? {
                    Some(w) => words.push(w),
                    None if self.at_end() => return incomplete("expected `)'"),
                    None => return self.unexpected(),
                }
            }
            items = Some(words);
        } else {
            self.skip_linebreaks();
            if self.peek_keyword() == Some("in") {
                self.pos += 2;
                let mut words = Vec::new();
                loop {
                    self.skip_blanks();
                    match self.peek() {
                        Some(';' | '\n') => {
                            self.pos += 1;
                            break;
                        }
                        None => return incomplete("expected `do'"),
                        _ => {}
                    }
                    match self.read_word()? {
                        Some(w) => words.push(w),
                        None => return self.unexpected(),
                    }
                }
                items = Some(words);
            } else {
                self.eat(";");
            }
        }
        self.skip_linebreaks();
        let body = self.parse_loop_body()?;
        Ok(Compound::For { var, items, body })
    }

    /// `do ... done`, or zsh's `{ ... }` / single command short forms.
    fn parse_loop_body(&mut self) -> PResult<List> {
        match self.peek_keyword() {
            Some("do") => self.parse_do_group(),
            _ => match self.parse_command()? {
                Some(c) => Ok(vec![simple_item(c)]),
                None if self.at_end() => incomplete("expected `do'"),
                None => self.unexpected(),
            },
        }
    }

    fn parse_case(&mut self) -> PResult<Compound> {
        self.skip_blanks();
        let Some(word) = self.read_word()? else { return self.unexpected() };
        self.expect_keyword("in")?;
        let mut arms = Vec::new();
        loop {
            self.skip_linebreaks();
            if self.peek_keyword() == Some("esac") {
                self.pos += 4;
                break;
            }
            if self.at_end() {
                return incomplete("expected `esac'");
            }
            self.eat("(");
            let mut patterns = Vec::new();
            loop {
                self.skip_blanks();
                match self.read_word()? {
                    Some(w) => patterns.push(w),
                    None => return self.unexpected(),
                }
                self.skip_blanks();
                if self.eat("|") {
                    continue;
                }
                if self.eat(")") {
                    break;
                }
                return self.unexpected();
            }
            let body = self.parse_list(false)?;
            self.skip_linebreaks();
            let fallthrough = if self.eat(";;") {
                false
            } else if self.eat(";&") || self.eat(";|") {
                true
            } else if self.peek_keyword() == Some("esac") {
                false
            } else if self.at_end() {
                return incomplete("expected `esac'");
            } else {
                return self.unexpected();
            };
            arms.push(CaseArm { patterns, body, fallthrough });
        }
        Ok(Compound::Case { word, arms })
    }

    fn parse_cond(&mut self) -> PResult<Compound> {
        let mut toks = Vec::new();
        loop {
            self.skip_linebreaks();
            if self.at_end() {
                return incomplete("expected `]]'");
            }
            if self.peek_keyword() == Some("]]") {
                self.pos += 2;
                break;
            }
            let op = ["&&", "||", "(", ")", "!", "<", ">"].iter().find(|op| self.starts_with(op));
            if let Some(op) = op {
                // `!` and `(` only as standalone tokens.
                if *op != "!" || self.peek_at(1).is_none_or(|c| c.is_whitespace()) {
                    self.pos += op.len();
                    toks.push(CondTok::Op(op.to_string()));
                    continue;
                }
            }
            let regex_rhs = matches!(toks.last(), Some(CondTok::Word(w)) if w.as_plain().as_deref() == Some("=~"));
            let w = if regex_rhs { self.read_regex_word()? } else { self.read_word()? };
            match w {
                Some(w) => toks.push(CondTok::Word(w)),
                None => return self.unexpected(),
            }
        }
        Ok(Compound::Cond(toks))
    }

    /// The right-hand side of `=~`: parentheses and `|` are part of the word.
    fn read_regex_word(&mut self) -> PResult<Option<Word>> {
        let start = self.pos;
        let mut depth = 0usize;
        let mut parts = Vec::new();
        let mut lit = String::new();
        while let Some(c) = self.peek() {
            match c {
                '(' => depth += 1,
                ')' if depth > 0 => depth -= 1,
                ' ' | '\t' | '\n' if depth == 0 => break,
                ')' | '&' | '<' | '>' | ';' if depth == 0 => break,
                '|' if depth == 0 && self.peek_at(1) == Some('|') => break,
                '\\' => {
                    self.pos += 1;
                    if let Some(n) = self.next() {
                        lit.push('\\');
                        lit.push(n);
                    }
                    continue;
                }
                '\'' | '"' | '$' => {
                    if !lit.is_empty() {
                        parts.push(Part::Lit(std::mem::take(&mut lit)));
                    }
                    let mut w = self.read_word_until(&|c| c.is_whitespace() || c == '(' || c == ')' || c == '|')?;
                    parts.append(&mut w.0);
                    continue;
                }
                _ => {}
            }
            lit.push(c);
            self.pos += 1;
        }
        if !lit.is_empty() {
            parts.push(Part::Lit(lit));
        }
        Ok(if self.pos == start { None } else { Some(Word(parts)) })
    }

    fn parse_simple(&mut self) -> PResult<Option<Command>> {
        let mut cmd = Simple::default();
        loop {
            self.skip_blanks();
            if let Some(r) = self.try_redir()? {
                cmd.redirs.push(r);
                continue;
            }
            let at_cmd_word = cmd.words.is_empty();
            let start = self.pos;
            let Some(word) = self.read_word()? else { break };
            if at_cmd_word && let Some(a) = self.try_assign(&word)? {
                cmd.assigns.push(a);
                continue;
            }
            if !at_cmd_word
                && self.peek() == Some('(')
                && let Some(arr) = self.try_decl_array(&cmd, &word)?
            {
                cmd.words.extend(arr);
                continue;
            }
            if at_cmd_word && let Some(name) = word.as_plain() {
                if self.try_alias(&name, start) {
                    continue;
                }
                // name() body
                let save = self.pos;
                self.skip_blanks();
                if cmd.assigns.is_empty() && cmd.redirs.is_empty() && self.eat("(") {
                    self.skip_blanks();
                    if self.eat(")") {
                        return self.parse_func_body(name).map(Some);
                    }
                }
                self.pos = save;
            }
            cmd.words.push(word);
        }
        if cmd.words.is_empty() && cmd.assigns.is_empty() && cmd.redirs.is_empty() {
            return Ok(None);
        }
        Ok(Some(Command::Simple(cmd)))
    }

    /// `local a=(x y)`: array literals in declaration arguments become
    /// marker words the declaration builtins decode.
    fn try_decl_array(&mut self, cmd: &Simple, word: &Word) -> PResult<Option<Vec<Word>>> {
        let is_decl = cmd.words[0].as_plain().is_some_and(|n| crate::builtins::is_declaration(&n));
        let Some(text) = word.as_plain() else { return Ok(None) };
        let Some(name) = text.strip_suffix('=') else { return Ok(None) };
        if !is_decl || !is_name(name) {
            return Ok(None);
        }
        self.pos += 1;
        let mut words = vec![Word(vec![Part::Quoted(format!("{name}={}", crate::builtins::ARRAY_OPEN))])];
        loop {
            self.skip_linebreaks();
            if self.eat(")") {
                break;
            }
            match self.read_word()? {
                Some(w) => words.push(w),
                None if self.at_end() => return incomplete("expected `)'"),
                None => return self.unexpected(),
            }
        }
        words.push(Word(vec![Part::Quoted(crate::builtins::ARRAY_CLOSE.to_string())]));
        Ok(Some(words))
    }

    fn try_alias(&mut self, name: &str, start: usize) -> bool {
        let guarded = self.alias_guard.iter().any(|(n, end)| n == name && start < *end);
        let Some(value) = self.aliases.get(name).filter(|_| !guarded) else { return false };
        let chars: Vec<char> = value.chars().collect();
        let delta = chars.len() as isize - (self.pos - start) as isize;
        for g in &mut self.alias_guard {
            if g.1 > start {
                g.1 = (g.1 as isize + delta) as usize;
            }
        }
        self.alias_guard.retain(|g| g.1 > start);
        let len = chars.len();
        self.src.splice(start..self.pos, chars);
        self.alias_guard.push((name.to_string(), start + len));
        self.pos = start;
        true
    }

    /// `NAME=value`, `NAME+=value`, `NAME=(a b c)`.
    fn try_assign(&mut self, word: &Word) -> PResult<Option<Assign>> {
        let Some(Part::Lit(first)) = word.0.first() else { return Ok(None) };
        let Some(eq) = first.find('=') else { return Ok(None) };
        let (mut name, append) = (first[..eq].to_string(), first[..eq].ends_with('+'));
        if append {
            name.pop();
        }
        if !is_name(&name) {
            return Ok(None);
        }
        let mut rest = word.0.clone();
        let tail = first[eq + 1..].to_string();
        if tail.is_empty() {
            rest.remove(0);
        } else {
            rest[0] = Part::Lit(tail);
        }
        if rest.is_empty() && self.peek() == Some('(') {
            self.pos += 1;
            let mut words = Vec::new();
            loop {
                self.skip_linebreaks();
                if self.eat(")") {
                    break;
                }
                match self.read_word()? {
                    Some(w) => words.push(w),
                    None if self.at_end() => return incomplete("expected `)'"),
                    None => return self.unexpected(),
                }
            }
            return Ok(Some(Assign { name, append, value: AssignValue::Array(words) }));
        }
        Ok(Some(Assign { name, append, value: AssignValue::Scalar(Word(rest)) }))
    }

    fn try_redir(&mut self) -> PResult<Option<Redir>> {
        let save = self.pos;
        let mut fd = None;
        let digits: String = self.src[self.pos..].iter().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() && matches!(self.peek_at(digits.len()), Some('<' | '>')) {
            fd = digits.parse().ok();
            self.pos += digits.len();
        }
        let op = if self.eat("&>>") {
            RedirOp::AppendErr
        } else if self.eat("&>") {
            RedirOp::OutErr
        } else if self.eat("<<<") {
            RedirOp::HereString
        } else if self.eat("<<-") {
            return self.read_heredoc(fd, true).map(Some);
        } else if self.eat("<<") {
            return self.read_heredoc(fd, false).map(Some);
        } else if self.eat("<>") {
            RedirOp::ReadWrite
        } else if self.eat("<&") {
            RedirOp::DupIn
        } else if self.eat("<") {
            RedirOp::In
        } else if self.eat(">>") {
            RedirOp::Append
        } else if self.eat(">&") {
            self.skip_blanks();
            let is_fd = self.peek().is_some_and(|c| c.is_ascii_digit() || c == '-')
                && self.peek_at(1).is_none_or(is_meta);
            if is_fd || fd.is_some() { RedirOp::DupOut } else { RedirOp::OutErr }
        } else if self.eat(">|") || self.eat(">!") || self.eat(">") {
            RedirOp::Out
        } else {
            self.pos = save;
            return Ok(None);
        };
        self.skip_blanks();
        match self.read_word()? {
            Some(target) => Ok(Some(Redir { fd, op, target })),
            None if self.at_end() => incomplete("expected redirection target"),
            None => self.unexpected(),
        }
    }

    /// Read the delimiter, then cut the body out of the source (it starts on
    /// the line after the current one).
    fn read_heredoc(&mut self, fd: Option<u32>, strip_tabs: bool) -> PResult<Redir> {
        self.skip_blanks();
        let Some(delim_word) = self.read_word()? else { return self.unexpected() };
        let quoted = delim_word.has_quotes();
        let delim = word_text(&delim_word);
        let Some(nl) = (self.pos..self.src.len()).find(|&i| self.src[i] == '\n') else {
            return incomplete("here-document");
        };
        let mut i = nl + 1;
        let mut body = String::new();
        let mut found = false;
        while i < self.src.len() {
            let end = (i..self.src.len()).find(|&j| self.src[j] == '\n').unwrap_or(self.src.len());
            let mut line: String = self.src[i..end].iter().collect();
            if strip_tabs {
                line = line.trim_start_matches('\t').to_string();
            }
            let next = (end + 1).min(self.src.len());
            if line == delim {
                i = next;
                found = true;
                break;
            }
            body.push_str(&line);
            body.push('\n');
            i = next;
        }
        if !found {
            return incomplete("here-document");
        }
        self.src.drain(nl + 1..i);
        let target = if quoted { Word(vec![Part::Quoted(body)]) } else { parse_heredoc_body(&body)? };
        Ok(Redir { fd, op: RedirOp::HereDoc, target })
    }

    // ---- words ----

    fn read_word(&mut self) -> PResult<Option<Word>> {
        let start = self.pos;
        let w = self.read_word_until(&is_meta)?;
        Ok(if self.pos == start { None } else { Some(w) })
    }

    fn read_word_until(&mut self, stop: &dyn Fn(char) -> bool) -> PResult<Word> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let flush = |lit: &mut String, parts: &mut Vec<Part>| {
            if !lit.is_empty() {
                parts.push(Part::Lit(std::mem::take(lit)));
            }
        };
        while let Some(c) = self.peek() {
            if stop(c) {
                break;
            }
            match c {
                '\\' => {
                    self.pos += 1;
                    match self.next() {
                        None => return incomplete("trailing backslash"),
                        Some('\n') => {}
                        Some(e) => {
                            flush(&mut lit, &mut parts);
                            parts.push(Part::Quoted(e.to_string()));
                        }
                    }
                }
                '\'' => {
                    flush(&mut lit, &mut parts);
                    self.pos += 1;
                    parts.push(Part::Quoted(self.read_squote()?));
                }
                '"' => {
                    flush(&mut lit, &mut parts);
                    self.pos += 1;
                    parts.push(Part::Double(self.read_dquote()?));
                }
                '`' => {
                    flush(&mut lit, &mut parts);
                    self.pos += 1;
                    parts.push(self.read_backtick()?);
                }
                '$' => {
                    self.pos += 1;
                    match self.read_dollar(false)? {
                        Some(p) => {
                            flush(&mut lit, &mut parts);
                            parts.push(p);
                        }
                        None => lit.push('$'),
                    }
                }
                _ => {
                    lit.push(c);
                    self.pos += 1;
                }
            }
        }
        flush(&mut lit, &mut parts);
        Ok(Word(parts))
    }

    fn read_squote(&mut self) -> PResult<String> {
        let mut s = String::new();
        loop {
            match self.next() {
                None => return incomplete("unterminated quote"),
                Some('\'') => return Ok(s),
                Some(c) => s.push(c),
            }
        }
    }

    /// `$'...'` with C escapes.
    fn read_ansi_quote(&mut self) -> PResult<String> {
        let mut s = String::new();
        loop {
            match self.next() {
                None => return incomplete("unterminated quote"),
                Some('\'') => return Ok(s),
                Some('\\') => {
                    let Some(e) = self.next() else { return incomplete("unterminated quote") };
                    match e {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        'r' => s.push('\r'),
                        'a' => s.push('\x07'),
                        'b' => s.push('\x08'),
                        'e' | 'E' => s.push('\x1b'),
                        'f' => s.push('\x0c'),
                        'v' => s.push('\x0b'),
                        '0'..='7' => {
                            let mut v = e.to_digit(8).unwrap();
                            for _ in 0..2 {
                                match self.peek().and_then(|c| c.to_digit(8)) {
                                    Some(d) => {
                                        v = v * 8 + d;
                                        self.pos += 1;
                                    }
                                    None => break,
                                }
                            }
                            s.push(char::from_u32(v).unwrap_or('?'));
                        }
                        'x' | 'u' | 'U' => {
                            let max = match e {
                                'x' => 2,
                                'u' => 4,
                                _ => 8,
                            };
                            let mut v = 0u32;
                            let mut n = 0;
                            while n < max {
                                match self.peek().and_then(|c| c.to_digit(16)) {
                                    Some(d) => {
                                        v = v * 16 + d;
                                        self.pos += 1;
                                        n += 1;
                                    }
                                    None => break,
                                }
                            }
                            s.push(char::from_u32(v).unwrap_or('?'));
                        }
                        other => s.push(other),
                    }
                }
                Some(c) => s.push(c),
            }
        }
    }

    fn read_dquote(&mut self) -> PResult<Vec<Part>> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        loop {
            let Some(c) = self.next() else { return incomplete("unterminated quote") };
            match c {
                '"' => break,
                '\\' => match self.next() {
                    None => return incomplete("unterminated quote"),
                    Some('\n') => {}
                    Some(e @ ('$' | '`' | '"' | '\\')) => lit.push(e),
                    Some(e) => {
                        lit.push('\\');
                        lit.push(e);
                    }
                },
                '`' => {
                    if !lit.is_empty() {
                        parts.push(Part::Quoted(std::mem::take(&mut lit)));
                    }
                    parts.push(self.read_backtick()?);
                }
                '$' => match self.read_dollar(true)? {
                    Some(p) => {
                        if !lit.is_empty() {
                            parts.push(Part::Quoted(std::mem::take(&mut lit)));
                        }
                        parts.push(p);
                    }
                    None => lit.push('$'),
                },
                c => lit.push(c),
            }
        }
        if !lit.is_empty() || parts.is_empty() {
            parts.push(Part::Quoted(lit));
        }
        Ok(parts)
    }

    fn read_backtick(&mut self) -> PResult<Part> {
        let mut s = String::new();
        loop {
            match self.next() {
                None => return incomplete("unterminated `"),
                Some('`') => break,
                Some('\\') => match self.next() {
                    Some(e @ ('`' | '\\' | '$')) => s.push(e),
                    Some(e) => {
                        s.push('\\');
                        s.push(e);
                    }
                    None => return incomplete("unterminated `"),
                },
                Some(c) => s.push(c),
            }
        }
        let list = Parser::parse_all(&s)?;
        Ok(Part::CmdSub(Arc::new(list)))
    }

    /// After a `$`. `None` means a literal dollar sign.
    fn read_dollar(&mut self, in_dquote: bool) -> PResult<Option<Part>> {
        let Some(c) = self.peek() else { return Ok(None) };
        match c {
            '(' if self.peek_at(1) == Some('(') => {
                self.pos += 2;
                let raw = self.read_arith_raw()?;
                Ok(Some(Part::Arith(parse_heredoc_body(&raw)?)))
            }
            '(' => {
                self.pos += 1;
                let list = self.parse_list(false)?;
                self.skip_linebreaks();
                if !self.eat(")") {
                    return if self.at_end() { incomplete("expected `)'") } else { self.unexpected() };
                }
                Ok(Some(Part::CmdSub(Arc::new(list))))
            }
            '[' => {
                // Old-style $[ arith ].
                self.pos += 1;
                let mut raw = String::new();
                loop {
                    match self.next() {
                        None => return incomplete("expected `]'"),
                        Some(']') => break,
                        Some(c) => raw.push(c),
                    }
                }
                Ok(Some(Part::Arith(parse_heredoc_body(&raw)?)))
            }
            '{' => {
                self.pos += 1;
                self.read_brace_param().map(Some)
            }
            '\'' if !in_dquote => {
                self.pos += 1;
                Ok(Some(Part::Quoted(self.read_ansi_quote()?)))
            }
            '"' if !in_dquote => {
                self.pos += 1;
                Ok(Some(Part::Double(self.read_dquote()?)))
            }
            c if is_name_start(c) => {
                let mut name = String::new();
                while let Some(c) = self.peek().filter(|&c| is_name_char(c)) {
                    name.push(c);
                    self.pos += 1;
                }
                let index = self.read_subscript()?;
                Ok(Some(Part::Param(Box::new(Param { name, length: false, index, op: None, flags: ParamFlags::default(), inner: None }))))
            }
            c if c.is_ascii_digit() || "?#@*$!-".contains(c) => {
                self.pos += 1;
                Ok(Some(Part::Param(Box::new(Param { name: c.to_string(), length: false, index: None, op: None, flags: ParamFlags::default(), inner: None }))))
            }
            _ => Ok(None),
        }
    }

    fn read_subscript(&mut self) -> PResult<Option<Word>> {
        if self.peek() != Some('[') {
            return Ok(None);
        }
        // Only if a matching `]` follows on this word.
        let mut depth = 0;
        let mut j = self.pos;
        while let Some(&c) = self.src.get(j) {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                c if c.is_whitespace() => return Ok(None),
                _ => {}
            }
            j += 1;
        }
        if j >= self.src.len() {
            return Ok(None);
        }
        self.pos += 1;
        let w = self.read_word_until(&|c| c == ']')?;
        self.pos += 1;
        Ok(Some(w))
    }

    fn read_brace_param(&mut self) -> PResult<Part> {
        let mut flags = ParamFlags::default();
        if self.peek() == Some('(') {
            self.pos += 1;
            loop {
                let Some(c) = self.next() else { return incomplete("expected `}'") };
                match c {
                    ')' => break,
                    'U' => flags.upper = true,
                    'L' => flags.lower = true,
                    '@' => flags.splat = true,
                    'f' => flags.split = Some("\n".into()),
                    'F' => flags.join = Some("\n".into()),
                    's' | 'j' => {
                        let Some(d) = self.next() else { return incomplete("expected `}'") };
                        let close = match d {
                            '(' => ')',
                            '[' => ']',
                            '{' => '}',
                            '<' => '>',
                            d => d,
                        };
                        let mut sep = String::new();
                        loop {
                            match self.next() {
                                None => return incomplete("expected `}'"),
                                Some(x) if x == close => break,
                                Some(x) => sep.push(x),
                            }
                        }
                        if c == 's' {
                            flags.split = Some(sep);
                        } else {
                            flags.join = Some(sep);
                        }
                    }
                    // Other zsh flags are accepted and ignored.
                    _ => {}
                }
            }
        }
        // `${=x}` splits on IFS; `${~x}` (glob subst) is accepted.
        loop {
            if self.eat("=") {
                flags.split = Some(String::new());
            } else if !self.eat("~") {
                break;
            }
        }
        let mut length = false;
        if self.peek() == Some('#') && !matches!(self.peek_at(1), Some('}')) {
            length = true;
            self.pos += 1;
        }
        let nested = match self.peek() {
            Some('$') if matches!(self.peek_at(1), Some('{' | '(')) => {
                self.pos += 1;
                self.read_dollar(true)?
            }
            Some('"') => {
                self.pos += 1;
                Some(Part::Double(self.read_dquote()?))
            }
            _ => None,
        };
        if let Some(inner) = nested {
            let Part::Param(mut p) = self.read_brace_param_rest(String::new(), length, flags)? else { unreachable!() };
            p.inner = Some(Box::new(inner));
            return Ok(Part::Param(p));
        }
        let mut name = String::new();
        match self.peek() {
            Some(c) if is_name_start(c) => {
                while let Some(c) = self.peek().filter(|&c| is_name_char(c)) {
                    name.push(c);
                    self.pos += 1;
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while let Some(c) = self.peek().filter(|c| c.is_ascii_digit()) {
                    name.push(c);
                    self.pos += 1;
                }
            }
            Some(c) if "?#@*$!-".contains(c) => {
                name.push(c);
                self.pos += 1;
            }
            None => return incomplete("expected `}'"),
            // zsh: `${:-word}` has no name.
            Some(':') => {}
            _ => return err("bad substitution"),
        }
        self.read_brace_param_rest(name, length, flags)
    }

    /// Subscript, operator and closing brace of `${...}`.
    fn read_brace_param_rest(&mut self, name: String, length: bool, flags: ParamFlags) -> PResult<Part> {
        let index = self.read_subscript()?;
        let word_to_brace = |p: &mut Parser| -> PResult<Word> { p.read_param_word(&|c| c == '}') };
        let c = self.peek();
        let op = match c {
            Some('}') => None,
            Some(':') => {
                self.pos += 1;
                match self.next() {
                    Some('-') => Some(ParamOp::Default { colon: true, word: word_to_brace(self)? }),
                    Some('=') => Some(ParamOp::Assign { colon: true, word: word_to_brace(self)? }),
                    Some('?') => Some(ParamOp::Error { colon: true, word: word_to_brace(self)? }),
                    Some('+') => Some(ParamOp::Alt { colon: true, word: word_to_brace(self)? }),
                    Some('u') if self.peek() == Some('}') => Some(ParamOp::Upper),
                    Some('l') if self.peek() == Some('}') => Some(ParamOp::Lower),
                    Some(_) => {
                        self.pos -= 1;
                        let offset = self.read_param_word(&|c| c == ':' || c == '}')?;
                        let length = if self.eat(":") { Some(word_to_brace(self)?) } else { None };
                        Some(ParamOp::Substr { offset, length })
                    }
                    None => return incomplete("expected `}'"),
                }
            }
            Some('-') | Some('=') | Some('?') | Some('+') => {
                self.pos += 1;
                let word = word_to_brace(self)?;
                Some(match c.unwrap() {
                    '-' => ParamOp::Default { colon: false, word },
                    '=' => ParamOp::Assign { colon: false, word },
                    '?' => ParamOp::Error { colon: false, word },
                    _ => ParamOp::Alt { colon: false, word },
                })
            }
            Some('#') => {
                self.pos += 1;
                let longest = self.eat("#");
                Some(ParamOp::TrimPrefix { longest, pat: word_to_brace(self)? })
            }
            Some('%') => {
                self.pos += 1;
                let longest = self.eat("%");
                Some(ParamOp::TrimSuffix { longest, pat: word_to_brace(self)? })
            }
            Some('/') => {
                self.pos += 1;
                let all = self.eat("/");
                let anchor = if !all && (self.peek() == Some('#') || self.peek() == Some('%')) { self.next() } else { None };
                let pat = self.read_param_word(&|c| c == '/' || c == '}')?;
                let rep = if self.eat("/") { word_to_brace(self)? } else { Word::default() };
                Some(ParamOp::Replace { all, anchor, pat, rep })
            }
            None => return incomplete("expected `}'"),
            _ => return err("bad substitution"),
        };
        if !self.eat("}") {
            return if self.at_end() { incomplete("expected `}'") } else { err("bad substitution") };
        }
        Ok(Part::Param(Box::new(Param { name, length, index, op, flags, inner: None })))
    }

    /// A word inside `${...}`: whitespace is literal, stops at `stop` (outside
    /// nested braces).
    fn read_param_word(&mut self, stop: &dyn Fn(char) -> bool) -> PResult<Word> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut depth = 0usize;
        loop {
            let Some(c) = self.peek() else { return incomplete("expected `}'") };
            if depth == 0 && stop(c) {
                break;
            }
            match c {
                '{' => {
                    depth += 1;
                    lit.push(c);
                    self.pos += 1;
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    lit.push(c);
                    self.pos += 1;
                }
                '\\' | '\'' | '"' | '`' | '$' => {
                    if !lit.is_empty() {
                        parts.push(Part::Lit(std::mem::take(&mut lit)));
                    }
                    let w = self.read_word_until(&|c| !matches!(c, '\\' | '\'' | '"' | '`' | '$'))?;
                    parts.extend(w.0);
                    // read_word_until stops after one construct only if the
                    // next char is ordinary; it may also have consumed
                    // ordinary chars following a `$` that wasn't an expansion.
                }
                _ => {
                    lit.push(c);
                    self.pos += 1;
                }
            }
        }
        if !lit.is_empty() {
            parts.push(Part::Lit(lit));
        }
        Ok(Word(parts))
    }

    /// Raw text up to the matching `))`.
    fn read_arith_raw(&mut self) -> PResult<String> {
        let mut depth = 0usize;
        let mut s = String::new();
        loop {
            match self.next() {
                None => return incomplete("expected `))'"),
                Some('(') => {
                    depth += 1;
                    s.push('(');
                }
                Some(')') if depth == 0 => {
                    if self.eat(")") {
                        return Ok(s);
                    }
                    return err("parse error: expected `))'");
                }
                Some(')') => {
                    depth -= 1;
                    s.push(')');
                }
                Some(c) => s.push(c),
            }
        }
    }
}

fn kw_static(kw: &str) -> &'static str {
    KEYWORDS.iter().find(|k| **k == kw).copied().unwrap_or("")
}

fn simple_item(c: Command) -> Item {
    Item {
        and_or: AndOr { first: Pipeline { negate: false, cmds: vec![c], stderr_too: vec![] }, rest: vec![] },
        background: false,
    }
}

/// Text of a word with quotes removed (for here-doc delimiters).
fn word_text(w: &Word) -> String {
    let mut s = String::new();
    for p in &w.0 {
        match p {
            Part::Lit(l) | Part::Quoted(l) => s.push_str(l),
            Part::Double(inner) => s.push_str(&word_text(&Word(inner.clone()))),
            Part::Param(p) => {
                s.push('$');
                s.push_str(&p.name);
            }
            _ => {}
        }
    }
    s
}

/// Parse text where only `$`, `` ` `` and `\` are special (here-doc bodies,
/// arithmetic). Literal text becomes `Quoted` parts.
pub fn parse_heredoc_body(body: &str) -> PResult<Word> {
    let mut p = Parser::new(body);
    let mut parts = Vec::new();
    let mut lit = String::new();
    while let Some(c) = p.next() {
        match c {
            '\\' => match p.peek() {
                Some(e @ ('$' | '`' | '\\')) => {
                    lit.push(e);
                    p.pos += 1;
                }
                Some('\n') => p.pos += 1,
                _ => lit.push('\\'),
            },
            '`' => {
                if !lit.is_empty() {
                    parts.push(Part::Quoted(std::mem::take(&mut lit)));
                }
                parts.push(p.read_backtick()?);
            }
            '$' => match p.read_dollar(true)? {
                Some(part) => {
                    if !lit.is_empty() {
                        parts.push(Part::Quoted(std::mem::take(&mut lit)));
                    }
                    parts.push(part);
                }
                None => lit.push('$'),
            },
            c => lit.push(c),
        }
    }
    if !lit.is_empty() || parts.is_empty() {
        parts.push(Part::Quoted(lit));
    }
    Ok(Word(parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> List {
        Parser::parse_all(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn simple(l: &List) -> &Simple {
        match &l[0].and_or.first.cmds[0] {
            Command::Simple(s) => s,
            c => panic!("not simple: {c:?}"),
        }
    }

    #[test]
    fn words_and_quotes() {
        let l = parse("echo a'b c'\"d $x\" \\e");
        let s = simple(&l);
        assert_eq!(s.words.len(), 3);
        assert!(matches!(&s.words[1].0[0], Part::Lit(a) if a == "a"));
        assert!(matches!(&s.words[1].0[1], Part::Quoted(b) if b == "b c"));
    }

    #[test]
    fn pipelines_and_lists() {
        let l = parse("a | b |& c && d || e; f &\ng");
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].and_or.first.cmds.len(), 3);
        assert_eq!(l[0].and_or.first.stderr_too, vec![false, true]);
        assert_eq!(l[0].and_or.rest.len(), 2);
        assert!(l[1].background);
    }

    #[test]
    fn compounds() {
        parse("if a; then b; elif c; then d; else e; fi");
        parse("while true; do echo; done");
        parse("for i in 1 2 3; do echo $i; done");
        parse("for i (1 2 3) echo $i");
        parse("for ((i=0; i<3; i++)); do echo $i; done");
        parse("case $x in a|b) echo ab;; (c) echo c;& *) echo d; esac");
        parse("f() { echo hi; }");
        parse("function g { echo; }");
        parse("{ a; b; } > out");
        parse("(cd x; ls) | wc");
        parse("[[ -n $x && $y == a* || ! -f z ]]");
        parse("[[ $x =~ ^(a|b)$ ]]");
        parse("(( x = 1 + 2 ))");
        parse("repeat 3 echo hi");
        parse("x=(a b c) y+=d z= cmd");
    }

    #[test]
    fn incomplete_inputs() {
        for s in ["if true; then", "echo 'abc", "for i in a b", "echo $(ls", "f() {", "cat <<EOF\nhi", "echo \\"] {
            let e = Parser::parse_all(s).unwrap_err();
            assert!(e.incomplete, "{s}: {e}");
        }
        assert!(!Parser::parse_all("echo )").unwrap_err().incomplete);
    }

    #[test]
    fn heredoc() {
        let l = parse("cat <<EOF; echo after\nhello $x\nEOF\necho next");
        assert_eq!(l.len(), 3);
        let s = simple(&l);
        assert_eq!(s.redirs[0].op, RedirOp::HereDoc);
    }

    #[test]
    fn params() {
        let l = parse("echo ${x:-def} ${#y} ${z%%.*} ${a/b/c} $arr[2] ${(U)n} ${v:1:2}");
        assert_eq!(simple(&l).words.len(), 8);
    }

    #[test]
    fn aliases() {
        let mut p = Parser::new("ll -a; ls");
        let mut al = HashMap::new();
        al.insert("ll".to_string(), "ls -l".to_string());
        al.insert("ls".to_string(), "ls -G".to_string());
        let l = p.next_command(&al).unwrap().unwrap();
        let words: Vec<_> = match &l[0].and_or.first.cmds[0] {
            Command::Simple(s) => s.words.iter().map(|w| w.as_plain().unwrap()).collect(),
            _ => unreachable!(),
        };
        assert_eq!(words, ["ls", "-G", "-l", "-a"]);
    }
}
