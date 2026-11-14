//! Syntax tree produced by the parser.

use std::sync::Arc;

/// Commands separated by `;`, `&` or newlines.
pub type List = Vec<Item>;

#[derive(Debug, Clone)]
pub struct Item {
    pub and_or: AndOr,
    pub background: bool,
}

#[derive(Debug, Clone)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Connector, Pipeline)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connector {
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub negate: bool,
    /// `true` for each `|&` link (stderr is piped too), one per gap.
    pub cmds: Vec<Command>,
    pub stderr_too: Vec<bool>,
}

#[derive(Debug, Clone)]
pub enum Command {
    Simple(Simple),
    Compound(Box<Compound>, Vec<Redir>),
    FuncDef(String, Arc<Command>),
}

#[derive(Debug, Clone, Default)]
pub struct Simple {
    pub assigns: Vec<Assign>,
    pub words: Vec<Word>,
    pub redirs: Vec<Redir>,
}

#[derive(Debug, Clone)]
pub struct Assign {
    pub name: String,
    pub append: bool,
    pub value: AssignValue,
}

#[derive(Debug, Clone)]
pub enum AssignValue {
    Scalar(Word),
    Array(Vec<Word>),
}

#[derive(Debug, Clone)]
pub enum Compound {
    Brace(List),
    Subshell(List),
    If { arms: Vec<(List, List)>, otherwise: Option<List> },
    While { cond: List, body: List, until: bool },
    For { var: String, items: Option<Vec<Word>>, body: List },
    /// `for ((init; cond; step))`
    ForArith { init: String, cond: String, step: String, body: List },
    Repeat { count: Word, body: List },
    Case { word: Word, arms: Vec<CaseArm> },
    /// `[[ ... ]]`: tokens, operators kept as `CondTok::Op`.
    Cond(Vec<CondTok>),
    /// `(( ... ))`
    Arith(String),
}

#[derive(Debug, Clone)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: List,
    /// `;&` falls through into the next arm's body.
    pub fallthrough: bool,
}

#[derive(Debug, Clone)]
pub enum CondTok {
    Word(Word),
    Op(String),
}

#[derive(Debug, Clone)]
pub struct Redir {
    pub fd: Option<u32>,
    pub op: RedirOp,
    pub target: Word,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirOp {
    /// `<`
    In,
    /// `>` and `>|`
    Out,
    /// `>>`
    Append,
    /// `<>`
    ReadWrite,
    /// `>&N` / `<&N` (target is the fd number or `-`)
    DupOut,
    DupIn,
    /// `&>` and `>&file`
    OutErr,
    /// `&>>`
    AppendErr,
    /// `<<` / `<<-` (target is the already-read body)
    HereDoc,
    /// `<<<`
    HereString,
}

#[derive(Debug, Clone, Default)]
pub struct Word(pub Vec<Part>);

#[derive(Debug, Clone)]
pub enum Part {
    /// Unquoted text: subject to globbing, brace and tilde expansion.
    Lit(String),
    /// Quoted text, taken literally (`'..'`, `$'..'`, `\x`).
    Quoted(String),
    /// `"..."`
    Double(Vec<Part>),
    Param(Box<Param>),
    CmdSub(Arc<List>),
    Arith(Word),
}

#[derive(Debug, Clone)]
pub struct Param {
    /// Variable name, digit string or one of `? # @ * $ ! - 0`.
    pub name: String,
    /// `${#name}`
    pub length: bool,
    /// `${name[idx]}` / `$name[idx]`
    pub index: Option<Word>,
    pub op: Option<ParamOp>,
    pub flags: ParamFlags,
    /// zsh nested substitution: `${${x#a}%b}`, `${(f)$(cmd)}` (name is empty).
    pub inner: Option<Box<Part>>,
}

/// zsh expansion flags: `${(s:,:)x}`, `${(j:-:)a}`, `${(f)x}`, `${=x}`, `${(@)a}`.
#[derive(Debug, Clone, Default)]
pub struct ParamFlags {
    /// Split the value into an array (`Some("")` = on IFS).
    pub split: Option<String>,
    pub join: Option<String>,
    /// Keep elements separate inside double quotes.
    pub splat: bool,
    pub upper: bool,
    pub lower: bool,
}

#[derive(Debug, Clone)]
pub enum ParamOp {
    /// `-` `=` `?` `+`, with `colon` meaning "also when empty".
    Default { colon: bool, word: Word },
    Assign { colon: bool, word: Word },
    Error { colon: bool, word: Word },
    Alt { colon: bool, word: Word },
    /// `#` / `##`
    TrimPrefix { longest: bool, pat: Word },
    /// `%` / `%%`
    TrimSuffix { longest: bool, pat: Word },
    /// `/pat/rep`, `//pat/rep`, `/#pat/rep`, `/%pat/rep`
    Replace { all: bool, anchor: Option<char>, pat: Word, rep: Word },
    /// `:offset[:length]`
    Substr { offset: Word, length: Option<Word> },
    /// zsh-style case flags `${(U)x}`, `${(L)x}` and `${x:u}`-like modifiers.
    Upper,
    Lower,
}

impl Word {
    #[cfg(test)]
    pub fn lit(s: &str) -> Word {
        Word(vec![Part::Lit(s.to_string())])
    }

    /// The word as plain text if it contains only unquoted literals.
    pub fn as_plain(&self) -> Option<String> {
        let mut s = String::new();
        for p in &self.0 {
            match p {
                Part::Lit(l) => s.push_str(l),
                _ => return None,
            }
        }
        Some(s)
    }

    /// True if any part is quoted (so `''` is a real empty argument).
    pub fn has_quotes(&self) -> bool {
        self.0.iter().any(|p| matches!(p, Part::Quoted(_) | Part::Double(_)))
    }
}
