//! Arithmetic in geometry options: `iw/2`, `(ih-oh)/2`, `-2`.
//!
//! Numbers, the variables a filter allows, unary `+` and `-`, the four
//! operations with the usual precedence, and parentheses. Expressions are
//! parsed when the filter is created, so a typo fails before any frame is
//! read, and evaluated for each input size.

use crate::error::{Error, Result};

/// Longest expression accepted; option values are short in practice and
/// the cap bounds the parser's work.
const MAX_LEN: usize = 1024;
/// Deepest nesting of parentheses and unary signs, which bounds recursion.
const MAX_DEPTH: usize = 64;

/// Values an expression can refer to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Var {
    /// Input width (`iw`, `in_w`).
    InW,
    /// Input height (`ih`, `in_h`).
    InH,
    /// Output width (`ow`, `out_w`).
    OutW,
    /// Output height (`oh`, `out_h`).
    OutH,
    /// Input aspect ratio, `iw / ih` (`a`).
    Aspect,
}

impl Var {
    fn name(self) -> &'static str {
        match self {
            Var::InW => "iw",
            Var::InH => "ih",
            Var::OutW => "ow",
            Var::OutH => "oh",
            Var::Aspect => "a",
        }
    }

    fn from_name(name: &str) -> Option<Var> {
        Some(match name {
            "iw" | "in_w" => Var::InW,
            "ih" | "in_h" => Var::InH,
            "ow" | "out_w" => Var::OutW,
            "oh" | "out_h" => Var::OutH,
            "a" => Var::Aspect,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Num(f64),
    Var(Var),
    Neg(Box<Node>),
    Bin(Op, Box<Node>, Box<Node>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    text: String,
    root: Node,
}

impl Expr {
    /// Parses `text`, accepting only the variables in `allowed`.
    pub fn parse(text: &str, allowed: &[Var]) -> Result<Expr> {
        if text.len() > MAX_LEN {
            return Err(Error::invalid_argument("expression is too long"));
        }
        let tokens = tokenize(text)?;
        let mut parser = Parser {
            text,
            tokens: &tokens,
            pos: 0,
            depth: 0,
            allowed,
        };
        let root = parser.expr()?;
        if parser.pos != tokens.len() {
            return Err(parser.error("unexpected input after the expression"));
        }
        Ok(Expr {
            text: text.to_string(),
            root,
        })
    }

    /// Whether the expression refers to `var`.
    pub fn uses(&self, var: Var) -> bool {
        fn walk(n: &Node, var: Var) -> bool {
            match n {
                Node::Num(_) => false,
                Node::Var(v) => *v == var,
                Node::Neg(a) => walk(a, var),
                Node::Bin(_, a, b) => walk(a, var) || walk(b, var),
            }
        }
        walk(&self.root, var)
    }

    /// Evaluates with `lookup` giving the value of each variable, or
    /// `None` when it is not known yet.
    pub fn eval(&self, lookup: &dyn Fn(Var) -> Option<f64>) -> Result<f64> {
        let v = self.eval_node(&self.root, lookup)?;
        if !v.is_finite() {
            return Err(self.error("result is not a finite number"));
        }
        Ok(v)
    }

    fn eval_node(&self, n: &Node, lookup: &dyn Fn(Var) -> Option<f64>) -> Result<f64> {
        Ok(match n {
            Node::Num(v) => *v,
            Node::Var(v) => {
                lookup(*v).ok_or_else(|| self.error(&format!("{} is not known here", v.name())))?
            }
            Node::Neg(a) => -self.eval_node(a, lookup)?,
            Node::Bin(op, a, b) => {
                let (a, b) = (self.eval_node(a, lookup)?, self.eval_node(b, lookup)?);
                match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div if b == 0.0 => return Err(self.error("division by zero")),
                    Op::Div => a / b,
                }
            }
        })
    }

    fn error(&self, msg: &str) -> Error {
        Error::invalid_argument(format!("expression '{}': {msg}", self.text))
    }
}

/// Evaluates a pair of width and height expressions, one of which may
/// refer to the other's result (`w=oh*2:h=240`), in the order that makes
/// that possible.
pub fn solve_size(w: &Expr, h: &Expr, iw: u32, ih: u32) -> Result<(f64, f64)> {
    let base = move |v: Var| match v {
        Var::InW => Some(iw as f64),
        Var::InH => Some(ih as f64),
        Var::Aspect => Some(iw as f64 / ih.max(1) as f64),
        Var::OutW | Var::OutH => None,
    };
    if w.uses(Var::OutH) && h.uses(Var::OutW) {
        return Err(Error::invalid_argument(
            "width and height expressions refer to each other",
        ));
    }
    if w.uses(Var::OutH) {
        let hv = h.eval(&base)?;
        let wv = w.eval(&|v| if v == Var::OutH { Some(hv) } else { base(v) })?;
        Ok((wv, hv))
    } else {
        let wv = w.eval(&base)?;
        let hv = h.eval(&|v| if v == Var::OutW { Some(wv) } else { base(v) })?;
        Ok((wv, hv))
    }
}

/// Variable lookup once input and output sizes are both known.
pub fn sizes(iw: u32, ih: u32, ow: u32, oh: u32) -> impl Fn(Var) -> Option<f64> {
    move |v| {
        Some(match v {
            Var::InW => iw as f64,
            Var::InH => ih as f64,
            Var::OutW => ow as f64,
            Var::OutH => oh as f64,
            Var::Aspect => iw as f64 / ih.max(1) as f64,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Num(f64),
    Ident(String),
    Sym(char),
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let bad = |msg: String| Error::invalid_argument(format!("expression '{text}': {msg}"));
    let mut tokens = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            let v: f64 = s
                .parse()
                .map_err(|_| bad(format!("invalid number '{s}'")))?;
            tokens.push(Token::Num(v));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
        } else if "+-*/()".contains(c) {
            tokens.push(Token::Sym(c));
            i += 1;
        } else {
            return Err(bad(format!("unexpected character '{c}'")));
        }
    }
    Ok(tokens)
}

/// Recursive descent over the grammar:
///
/// ```text
/// expr  = term { ("+" | "-") term }
/// term  = unary { ("*" | "/") unary }
/// unary = ("+" | "-") unary | primary
/// primary = number | variable | "(" expr ")"
/// ```
struct Parser<'a> {
    text: &'a str,
    tokens: &'a [Token],
    pos: usize,
    depth: usize,
    allowed: &'a [Var],
}

impl Parser<'_> {
    fn error(&self, msg: &str) -> Error {
        Error::invalid_argument(format!("expression '{}': {msg}", self.text))
    }

    fn peek_sym(&self) -> Option<char> {
        match self.tokens.get(self.pos) {
            Some(Token::Sym(c)) => Some(*c),
            _ => None,
        }
    }

    fn expr(&mut self) -> Result<Node> {
        let mut left = self.term()?;
        while let Some(c @ ('+' | '-')) = self.peek_sym() {
            self.pos += 1;
            let right = self.term()?;
            let op = if c == '+' { Op::Add } else { Op::Sub };
            left = Node::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Node> {
        let mut left = self.unary()?;
        while let Some(c @ ('*' | '/')) = self.peek_sym() {
            self.pos += 1;
            let right = self.unary()?;
            let op = if c == '*' { Op::Mul } else { Op::Div };
            left = Node::Bin(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Node> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        let node = match self.peek_sym() {
            Some('-') => {
                self.pos += 1;
                Node::Neg(Box::new(self.unary()?))
            }
            Some('+') => {
                self.pos += 1;
                self.unary()?
            }
            _ => self.primary()?,
        };
        self.depth -= 1;
        Ok(node)
    }

    fn primary(&mut self) -> Result<Node> {
        let token = self
            .tokens
            .get(self.pos)
            .ok_or_else(|| self.error("unexpected end"))?;
        self.pos += 1;
        match token {
            Token::Num(v) => Ok(Node::Num(*v)),
            Token::Ident(name) => match Var::from_name(name) {
                Some(v) if self.allowed.contains(&v) => Ok(Node::Var(v)),
                _ => Err(self.error(&format!("unknown name '{name}'"))),
            },
            Token::Sym('(') => {
                let inner = self.expr()?;
                if self.peek_sym() != Some(')') {
                    return Err(self.error("missing ')'"));
                }
                self.pos += 1;
                Ok(inner)
            }
            Token::Sym(c) => Err(self.error(&format!("unexpected '{c}'"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[Var] = &[Var::InW, Var::InH, Var::OutW, Var::OutH, Var::Aspect];

    fn eval(s: &str) -> Result<f64> {
        Expr::parse(s, ALL)?.eval(&sizes(320, 240, 160, 90))
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(eval("1+2*3").unwrap(), 7.0);
        assert_eq!(eval("(1+2)*3").unwrap(), 9.0);
        assert_eq!(eval("10-4-3").unwrap(), 3.0);
        assert_eq!(eval("24/4/2").unwrap(), 3.0);
        assert_eq!(eval("-2").unwrap(), -2.0);
        assert_eq!(eval("--2").unwrap(), 2.0);
        assert_eq!(eval("+3 * -2").unwrap(), -6.0);
        assert_eq!(eval(" 1.5 * 2 ").unwrap(), 3.0);
        assert_eq!(eval(".5*4").unwrap(), 2.0);
    }

    #[test]
    fn variables() {
        assert_eq!(eval("iw/2").unwrap(), 160.0);
        assert_eq!(eval("in_h*2").unwrap(), 480.0);
        assert_eq!(eval("(iw-ow)/2").unwrap(), 80.0);
        assert_eq!(eval("(in_h-out_h)/2").unwrap(), 75.0);
        assert!((eval("a").unwrap() - 4.0 / 3.0).abs() < 1e-12);
        // A variable the filter does not offer is a parse error.
        assert!(Expr::parse("ow", &[Var::InW]).is_err());
        assert!(Expr::parse("iw", &[Var::InW]).is_ok());
    }

    #[test]
    fn syntax_errors() {
        for s in [
            "", "1+", "(1", "1)", "2*/3", "iw iw", "w", "1..2", "3%2", "foo", "()",
        ] {
            assert!(eval(s).is_err(), "{s}");
        }
        assert!(eval("1/0").is_err());
        assert!(eval("1/(iw-320)").is_err());
        let deep = format!("{}1{}", "(".repeat(100), ")".repeat(100));
        assert!(Expr::parse(&deep, ALL).is_err());
        let minus = format!("{}1", "-".repeat(100));
        assert!(Expr::parse(&minus, ALL).is_err());
        let fine = format!("{}1{}", "(".repeat(20), ")".repeat(20));
        assert_eq!(
            Expr::parse(&fine, ALL).unwrap().eval(&|_| None).unwrap(),
            1.0
        );
        assert!(Expr::parse(&"1+".repeat(600), ALL).is_err());
    }

    #[test]
    fn unknown_values_are_errors() {
        let e = Expr::parse("ow*2", ALL).unwrap();
        assert!(e.uses(Var::OutW) && !e.uses(Var::InW));
        assert!(e.eval(&|_| None).is_err());
    }

    #[test]
    fn sizes_resolve_in_dependency_order() {
        let p = |s: &str| Expr::parse(s, ALL).unwrap();
        assert_eq!(
            solve_size(&p("iw/2"), &p("ow/2"), 320, 240).unwrap(),
            (160.0, 80.0)
        );
        assert_eq!(
            solve_size(&p("oh*2"), &p("ih/4"), 320, 240).unwrap(),
            (120.0, 60.0)
        );
        assert!(solve_size(&p("oh"), &p("ow"), 320, 240).is_err());
    }
}
