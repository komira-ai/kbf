//! GitHub Actions expressions, the part of them the workflows here write in a concurrency group,
//! a `cancel-in-progress` and a job's `if:`, evaluated for one merge queue run ([`Run`]).
//!
//! The grammar read: string literals (`'...'`, `''` for a quote), numbers, `true`, `false`,
//! `null`, context paths (`github.ref`), `!`, `==`, `!=`, `&&`, `||`, parentheses, and the
//! functions `format`, `always`, `success`, `failure` and `cancelled`. `&&` and `||` return
//! one of their operands, as GitHub's do, and stop at the first that decides; `==` compares
//! strings ignoring case and anything else as numbers. Anything else (an index, `<`, another
//! function, a context the run does not hold) is an error naming it, so the lint never guesses
//! what GitHub would make of it.

/// A value of an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

impl Val {
    /// Whether GitHub takes the value as true: `null`, `false`, `0`, `NaN` and `''` are false.
    pub fn truthy(&self) -> bool {
        match self {
            Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0 && !n.is_nan(),
            Val::Str(s) => !s.is_empty(),
        }
    }

    /// The value as `${{ }}` writes it into a string.
    fn text(&self) -> String {
        match self {
            Val::Null => String::new(),
            Val::Bool(b) => b.to_string(),
            Val::Num(n) => n.to_string(),
            Val::Str(s) => s.clone(),
        }
    }

    /// The value as a number, for `==` across types: `null` and `''` are 0, a string that is
    /// not a number is `NaN` (equal to nothing).
    fn number(&self) -> f64 {
        match self {
            Val::Null => 0.0,
            Val::Bool(b) => f64::from(u8::from(*b)),
            Val::Num(n) => *n,
            Val::Str(s) if s.trim().is_empty() => 0.0,
            Val::Str(s) => s.trim().parse().unwrap_or(f64::NAN),
        }
    }
}

/// GitHub's `==`.
fn equal(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Str(x), Val::Str(y)) => x.eq_ignore_ascii_case(y),
        _ => a.number() == b.number(),
    }
}

/// One merge queue run: the contexts it holds. Its event is `merge_group`; a run that no job
/// before has failed or been cancelled.
#[derive(Debug, Clone, Copy)]
pub struct Run<'a> {
    /// `github.ref`: the queue entry's own branch.
    pub git_ref: &'a str,
    /// `github.run_id`.
    pub run_id: &'a str,
    /// `github.sha`: the head of the entry's merge group.
    pub sha: &'a str,
}

impl Run<'_> {
    /// The value of the context path `path` (compared ignoring case, as GitHub does).
    fn context(&self, path: &str) -> Result<Val, String> {
        let lower = path.to_ascii_lowercase();
        let s = |v: &str| Ok(Val::Str(v.to_owned()));
        match lower.as_str() {
            "github.event_name" => s("merge_group"),
            "github.ref" => s(self.git_ref),
            "github.run_id" => s(self.run_id),
            "github.sha" => s(self.sha),
            // Set for a pull request alone.
            "github.base_ref" | "github.head_ref" => s(""),
            // A merge_group run is no pull request, and is not called: neither context is set.
            p if p.starts_with("github.event.pull_request.") || p.starts_with("inputs.") => {
                Ok(Val::Null)
            }
            _ => Err(format!(
                "`{path}`, which this lint does not model for a merge queue run"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Str(String),
    Num(f64),
    Ident(String),
    LParen,
    RParen,
    Comma,
    Not,
    Eq,
    Ne,
    And,
    Or,
}

fn tokens(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let two = |t: Tok| (t, 2);
        let (tok, len) = match (c, next) {
            (c, _) if c.is_whitespace() => {
                i += 1;
                continue;
            }
            ('\'', _) => {
                let mut s = String::new();
                let mut j = i + 1;
                loop {
                    match (chars.get(j), chars.get(j + 1)) {
                        (Some('\''), Some('\'')) => {
                            s.push('\'');
                            j += 2;
                        }
                        (Some('\''), _) => break,
                        (Some(&ch), _) => {
                            s.push(ch);
                            j += 1;
                        }
                        (None, _) => return Err(format!("an unterminated string in `{src}`")),
                    }
                }
                (Tok::Str(s), j + 1 - i)
            }
            ('(', _) => (Tok::LParen, 1),
            (')', _) => (Tok::RParen, 1),
            (',', _) => (Tok::Comma, 1),
            ('!', Some('=')) => two(Tok::Ne),
            ('!', _) => (Tok::Not, 1),
            ('=', Some('=')) => two(Tok::Eq),
            ('&', Some('&')) => two(Tok::And),
            ('|', Some('|')) => two(Tok::Or),
            (c, _) if c.is_ascii_digit() => {
                let len = chars[i..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit() || **c == '.')
                    .count();
                let text: String = chars[i..i + len].iter().collect();
                let n = text
                    .parse()
                    .map_err(|_| format!("the number `{text}` in `{src}`"))?;
                (Tok::Num(n), len)
            }
            (c, _) if c.is_ascii_alphabetic() || c == '_' => {
                let len = chars[i..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(**c, '_' | '-' | '.'))
                    .count();
                (Tok::Ident(chars[i..i + len].iter().collect()), len)
            }
            (c, _) => return Err(format!("`{c}` in `{src}`, which this lint does not read")),
        };
        out.push(tok);
        i += len;
    }
    Ok(out)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    at: usize,
    run: &'a Run<'a>,
    src: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn eat(&mut self, t: &Tok) -> bool {
        let found = self.peek() == Some(t);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect(&mut self, t: &Tok) -> Result<(), String> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(format!("`{}` is not a whole expression", self.src))
        }
    }

    /// `a || b`: the first true operand, or the last. `live` is false in an operand an earlier
    /// one decided: it is parsed, and nothing in it is looked up.
    fn or(&mut self, live: bool) -> Result<Val, String> {
        let mut v = self.and(live)?;
        while self.eat(&Tok::Or) {
            let decided = v.truthy();
            let r = self.and(live && !decided)?;
            if !decided {
                v = r;
            }
        }
        Ok(v)
    }

    fn and(&mut self, live: bool) -> Result<Val, String> {
        let mut v = self.equality(live)?;
        while self.eat(&Tok::And) {
            let decided = !v.truthy();
            let r = self.equality(live && !decided)?;
            if !decided {
                v = r;
            }
        }
        Ok(v)
    }

    fn equality(&mut self, live: bool) -> Result<Val, String> {
        let mut v = self.unary(live)?;
        loop {
            let negate = if self.eat(&Tok::Eq) {
                false
            } else if self.eat(&Tok::Ne) {
                true
            } else {
                return Ok(v);
            };
            let r = self.unary(live)?;
            v = Val::Bool(equal(&v, &r) != negate);
        }
    }

    fn unary(&mut self, live: bool) -> Result<Val, String> {
        if self.eat(&Tok::Not) {
            return Ok(Val::Bool(!self.unary(live)?.truthy()));
        }
        self.primary(live)
    }

    fn primary(&mut self, live: bool) -> Result<Val, String> {
        let tok = self
            .peek()
            .cloned()
            .ok_or_else(|| format!("`{}` ends where a value is expected", self.src))?;
        self.at += 1;
        match tok {
            Tok::Str(s) => Ok(Val::Str(s)),
            Tok::Num(n) => Ok(Val::Num(n)),
            Tok::LParen => {
                let v = self.or(live)?;
                self.expect(&Tok::RParen)?;
                Ok(v)
            }
            Tok::Ident(name) if self.eat(&Tok::LParen) => self.call(&name, live),
            Tok::Ident(name) => match name.to_ascii_lowercase().as_str() {
                "true" => Ok(Val::Bool(true)),
                "false" => Ok(Val::Bool(false)),
                "null" => Ok(Val::Null),
                _ if live => self.run.context(&name),
                _ => Ok(Val::Null),
            },
            _ => Err(format!("`{}` is not a whole expression", self.src)),
        }
    }

    /// A call of `name`, its `(` read. A function outside [`FUNCTIONS`] is refused by name
    /// before its arguments are read.
    fn call(&mut self, name: &str, live: bool) -> Result<Val, String> {
        if !FUNCTIONS.iter().any(|f| f.eq_ignore_ascii_case(name)) {
            return Err(format!(
                "the function `{name}`, which this lint does not read"
            ));
        }
        let mut args = Vec::new();
        if !self.eat(&Tok::RParen) {
            loop {
                args.push(self.or(live)?);
                if self.eat(&Tok::RParen) {
                    break;
                }
                self.expect(&Tok::Comma)?;
            }
        }
        let status = |v: bool| {
            if args.is_empty() {
                Ok(Val::Bool(v))
            } else {
                Err(format!("`{name}` takes no argument"))
            }
        };
        match name.to_ascii_lowercase().as_str() {
            // No job before has failed or been cancelled in the run this models.
            "always" | "success" => status(true),
            "failure" | "cancelled" => status(false),
            // `format`, the one of FUNCTIONS left.
            _ => {
                let Some((Val::Str(fmt), rest)) = args.split_first() else {
                    return Err("`format` takes a string first".to_owned());
                };
                fill(fmt, rest).map(Val::Str)
            }
        }
    }
}

/// The functions [`Parser::call`] evaluates.
const FUNCTIONS: [&str; 5] = ["always", "success", "failure", "cancelled", "format"];

/// GitHub's `format`: `{N}` is argument N, `{{` and `}}` are braces.
fn fill(fmt: &str, args: &[Val]) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = fmt;
    while let Some(c) = rest.chars().next() {
        if let Some(r) = rest.strip_prefix("{{") {
            out.push('{');
            rest = r;
        } else if let Some(r) = rest.strip_prefix("}}") {
            out.push('}');
            rest = r;
        } else if let Some(r) = rest.strip_prefix('{') {
            let close = r
                .find('}')
                .ok_or_else(|| format!("an unclosed `{{` in `{fmt}`"))?;
            let arg = r[..close]
                .parse::<usize>()
                .ok()
                .and_then(|n| args.get(n))
                .ok_or_else(|| format!("`{{{}}}` names no argument of `{fmt}`", &r[..close]))?;
            out.push_str(&arg.text());
            rest = &r[close + 1..];
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    Ok(out)
}

/// The value of the expression `src` (no `${{ }}` around it) in `run`.
pub fn eval(src: &str, run: &Run<'_>) -> Result<Val, String> {
    let mut p = Parser {
        toks: tokens(src)?,
        at: 0,
        run,
        src,
    };
    let v = p.or(true)?;
    if p.peek().is_some() {
        return Err(format!("`{src}` is not a whole expression"));
    }
    Ok(v)
}

/// The end of the expression that starts at `from` in `s` (the index of its `}}`), skipping
/// string literals.
fn close_of(s: &str, from: usize) -> Option<usize> {
    let mut quoted = false;
    let bytes = s.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => quoted = !quoted,
            b'}' if !quoted && bytes.get(i + 1) == Some(&b'}') => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// A string with `${{ }}` in it, each expression written in as its value.
pub fn template(s: &str, run: &Run<'_>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find("${{") {
        out.push_str(&rest[..open]);
        let close =
            close_of(rest, open + 3).ok_or_else(|| format!("an unclosed `${{{{` in `{s}`"))?;
        out.push_str(&eval(&rest[open + 3..close], run)?.text());
        rest = &rest[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The value of a key GitHub reads as an expression whether or not `${{ }}` is around it (a
/// job's `if:`): the one expression, or the bare text.
pub fn condition(s: &str, run: &Run<'_>) -> Result<bool, String> {
    let t = s.trim();
    let inner = match t.strip_prefix("${{") {
        Some(r) if close_of(r, 0) == Some(r.len().saturating_sub(2)) => &r[..r.len() - 2],
        Some(_) => return Err(format!("`{s}` mixes text and `${{{{ }}}}`")),
        None => t,
    };
    Ok(eval(inner, run)?.truthy())
}

/// A `cancel-in-progress`: `true` or `false`, or one `${{ }}` expression. `(absent)` (no key,
/// as `workflows::concurrency` gives it) is false.
pub fn flag(s: &str, run: &Run<'_>) -> Result<bool, String> {
    match s.trim() {
        "(absent)" | "false" => Ok(false),
        "true" => Ok(true),
        t if t.starts_with("${{") => condition(t, run),
        t => Err(format!("`{t}` is neither a boolean nor an expression")),
    }
}
