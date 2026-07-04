//! The hermetic expression tier (Decision 21b) — the *entire* computation budget of
//! the document language: parameters, arithmetic over integers and decimal-exact SI
//! quantities, comparisons, boolean logic, parenthesization, and references to named
//! params. Nothing else. There are, deliberately and by architectural invariant, **no
//! user-defined functions, no strings, no recursion, no loops, and no I/O** — the
//! Onshape clause (Decision 21c) forbids growing this into a language. Every expression
//! is pure, terminating, and evaluates in time linear in its own size. Nesting depth is
//! bounded ([`MAX_EXPR_DEPTH`]) so the recursive parser/evaluator cannot overflow the
//! native stack on a pathological input — an over-deep expression is an `E_EXPR` error,
//! never a process abort (the commit gate must degrade to a diagnostic, never crash).
//!
//! # Why a separate value type instead of reusing [`Quantity`]
//!
//! [`crate::quantity::Quantity`] is a *decimal-exact magnitude* (`mant × 10^exp`) with
//! no notion of whether it is a dimensionless count or a physical value, and no boolean.
//! The expression tier needs three things a raw `Quantity` cannot express: (1) a
//! **boolean** result (comparisons, `if=` variants); (2) the integer/quantity
//! distinction so `[0..n]` bounds are honestly integers and unit math stays a quantity;
//! (3) an evaluation that **rejects inexact division** rather than silently rounding —
//! the house rule that no float ever enters the model. So [`Value`] wraps `Quantity`
//! arithmetic but adds the type tags and the exactness gate.
//!
//! # Exactness
//!
//! All arithmetic rides `mant × 10^exp` `i64` math with checked overflow (`E_EXPR` on
//! overflow, never a wrap or a panic). Division is **exact-or-rejected**: `10 / 4`
//! (integers) and `1 / 3` (any) are `E_EXPR` because the quotient is not representable
//! as a terminating decimal `mant × 10^exp` — the author must write the value they mean
//! (`2.5`). `10 / 5 = 2` and `1 / 4 = 0.25` are exact and accepted. This is the same
//! "no silent float fallback" commitment the coordinate kernel and `quantity` make.

use crate::quantity::{Quantity, parse as parse_quantity};
use std::collections::BTreeMap;

/// A value in the expression tier. Integers and quantities are kept distinct so that
/// range bounds (`[0..n]`) can honestly demand an integer while unit-carrying arithmetic
/// stays a decimal-exact [`Quantity`]. Booleans are the comparison/logic result and the
/// `if=` variant condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    /// A dimensionless integer — a count, an index, a range bound. Exact `i64`.
    Int(i64),
    /// A decimal-exact quantity (`mant × 10^exp`), carrying an SI/IEC-authored value.
    /// Dimensionless in v1 (no unit-dimension algebra); the *spelling* of the unit is a
    /// display concern owned downstream (Decision 14), not tracked here.
    Quantity(Quantity),
    /// A boolean — the result of a comparison or a boolean operator, and the type an
    /// `if=` population conditional must evaluate to.
    Bool(bool),
}

impl Value {
    /// Interpret the value as a non-negative range bound: an [`Int`](Value::Int) (or an
    /// integer-valued [`Quantity`]) that is `≥ 0`. Booleans and fractional quantities are
    /// rejected. Used by range instantiation to turn a bound expression into a `usize`.
    pub fn as_index(self) -> Result<i64, String> {
        match self {
            Value::Int(n) => Ok(n),
            Value::Quantity(q) => q
                .as_integer()
                .ok_or_else(|| "range bound must be a whole number".to_string()),
            Value::Bool(_) => Err("range bound must be an integer, not a boolean".into()),
        }
    }

    /// Interpret the value as a boolean condition (the `if=` variant gate). Only a real
    /// [`Bool`](Value::Bool) qualifies — an integer is *not* silently truthy, keeping the
    /// language explicit (`if=(n > 0)`, never `if=n`).
    pub fn as_bool(self) -> Result<bool, String> {
        match self {
            Value::Bool(b) => Ok(b),
            _ => Err("condition must be a boolean (e.g. `n > 0`), not a number".into()),
        }
    }

    /// A human token for the value's type, for `E_EXPR` type-mismatch messages.
    fn type_name(self) -> &'static str {
        match self {
            Value::Int(_) => "integer",
            Value::Quantity(_) => "quantity",
            Value::Bool(_) => "boolean",
        }
    }
}

// ----------------------------------------------------------------------------
// Quantity helpers — decimal-exact arithmetic on `mant × 10^exp`
// ----------------------------------------------------------------------------

impl Quantity {
    /// This quantity as an exact `i64` if it has no fractional part, else `None`. Used to
    /// coerce an integer-valued quantity (`10k` = 10000) into a range bound.
    fn as_integer(self) -> Option<i64> {
        if self.exp >= 0 {
            let scale = pow10(self.exp as u32)?;
            self.mant.checked_mul(scale)
        } else {
            let scale = pow10((-self.exp) as u32)?;
            (self.mant % scale == 0).then_some(self.mant / scale)
        }
    }
}

/// `10^n` as an `i64`, or `None` on overflow. Small `n` in practice (an exponent past
/// ~18 overflows and is reported as `E_EXPR`, never wrapped).
fn pow10(n: u32) -> Option<i64> {
    10i64.checked_pow(n)
}

/// Rescale `q` to a target exponent `≤ q.exp` (more negative = finer), returning the new
/// mantissa, or `None` on overflow. Used to bring two quantities to a common exponent
/// before add/subtract/compare so the math stays exact.
fn rescale_mant(q: Quantity, target_exp: i32) -> Option<i64> {
    debug_assert!(target_exp <= q.exp);
    let shift = (q.exp - target_exp) as u32;
    q.mant.checked_mul(pow10(shift)?)
}

/// Bring two quantities to a common (minimum) exponent, returning `(mant_a, mant_b,
/// common_exp)`. Exact — the common exponent is the finer of the two, so neither loses
/// digits. `None` on overflow.
fn align(a: Quantity, b: Quantity) -> Option<(i64, i64, i32)> {
    let e = a.exp.min(b.exp);
    Some((rescale_mant(a, e)?, rescale_mant(b, e)?, e))
}

/// Normalize a quantity by stripping trailing-zero mantissa digits into the exponent
/// (`4700 × 10^0` → `47 × 10^2`), so equal values compare/format canonically and the
/// exactness check in division sees the minimal form. Zero normalizes to `0 × 10^0`.
fn normalize(mut q: Quantity) -> Quantity {
    if q.mant == 0 {
        return Quantity { mant: 0, exp: 0 };
    }
    while q.mant % 10 == 0 {
        q.mant /= 10;
        q.exp += 1;
    }
    q
}

// ----------------------------------------------------------------------------
// Environment
// ----------------------------------------------------------------------------

/// The evaluated parameter environment: name → value. Built once per elaboration by
/// resolving every `param` directive (with cycle detection) before any consumer
/// expression runs. Immutable during expression evaluation (no assignment operator
/// exists — params are declarations, not variables).
pub type Env = BTreeMap<String, Value>;

// ----------------------------------------------------------------------------
// Tokenizer
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(String),   // a literal — parsed to Int or Quantity at eval
    Ident(String), // a param reference, or a keyword (`true`/`false`)
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    Ne,
    And,
    Or,
    Not,
}

/// Split an expression string into tokens. Whitespace-insensitive. A literal number run
/// keeps its SI/IEC spelling intact (`4.7k`, `2R6`, `100nF`) so [`parse_quantity`] can
/// interpret it at eval; an identifier is an ASCII-alnum/underscore run. Operators are
/// the fixed set below. Any other character is an `E_EXPR` lex error.
fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '+' => {
                toks.push(Tok::Plus);
                i += 1;
            }
            '-' => {
                toks.push(Tok::Minus);
                i += 1;
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '/' => {
                toks.push(Tok::Slash);
                i += 1;
            }
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Le);
                    i += 2;
                } else {
                    toks.push(Tok::Lt);
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Ge);
                    i += 2;
                } else {
                    toks.push(Tok::Gt);
                    i += 1;
                }
            }
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::EqEq);
                    i += 2;
                } else {
                    return Err("stray `=` (did you mean `==`?)".into());
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Ne);
                    i += 2;
                } else {
                    toks.push(Tok::Not);
                    i += 1;
                }
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    toks.push(Tok::And);
                    i += 2;
                } else {
                    return Err("stray `&` (did you mean `&&`?)".into());
                }
            }
            '|' => {
                if chars.get(i + 1) == Some(&'|') {
                    toks.push(Tok::Or);
                    i += 2;
                } else {
                    return Err("stray `|` (did you mean `||`?)".into());
                }
            }
            _ if c.is_ascii_digit() || c == '.' => {
                // A numeric literal, in any spelling `quantity::parse` accepts (plain
                // decimal, SI multiplier, IEC letter, trailing unit). We grab a maximal
                // run of characters that could belong to such a literal — digits, a dot,
                // and letters (SI/IEC scale letters and unit tokens) — and hand the whole
                // run to `quantity::parse`, which validates it.
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                    i += 1;
                }
                toks.push(Tok::Num(chars[start..i].iter().collect()));
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                toks.push(Tok::Ident(chars[start..i].iter().collect()));
            }
            other => return Err(format!("unexpected character `{other}` in expression")),
        }
    }
    Ok(toks)
}

// ----------------------------------------------------------------------------
// Parser (recursive descent) + AST
// ----------------------------------------------------------------------------

/// The expression AST. Small and closed — no call node, no lambda, no let: the grammar
/// *cannot* express computation beyond the Decision-21b budget.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Num(String),
    Ref(String),
    Bool(bool),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

/// The maximum nesting depth of parenthesization / unary chains the parser and evaluator
/// accept. Recursive-descent parsing and recursive evaluation both consume native stack
/// per level; without a bound, a pathological input (e.g. 2000 nested parens) overflows
/// the stack and *aborts the process* — unacceptable for a commit gate that must degrade
/// to a diagnostic, never crash (Decision 21b). 64 is far beyond anything a human or
/// agent authors while leaving generous native-stack headroom. Enforced in `parse_unary`
/// / `parse_primary` (the two recursion points), in `eval` (recursive), and in
/// `collect_refs` (recursive) — every tree-walker that recurses on nesting.
pub const MAX_EXPR_DEPTH: u32 = 64;

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    /// Current parenthesization/unary nesting depth (see [`MAX_EXPR_DEPTH`]).
    depth: u32,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    // Precedence climbing, lowest to highest:
    //   or → and → equality → comparison → additive → multiplicative → unary → primary
    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_and()?;
        while self.eat(&Tok::Or) {
            let rhs = self.parse_and()?;
            lhs = Expr::Bin(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_equality()?;
        while self.eat(&Tok::And) {
            let rhs = self.parse_equality()?;
            lhs = Expr::Bin(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_equality(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Some(Tok::EqEq) => BinOp::Eq,
                Some(Tok::Ne) => BinOp::Ne,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_comparison()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Lt) => BinOp::Lt,
                Some(Tok::Le) => BinOp::Le,
                Some(Tok::Gt) => BinOp::Gt,
                Some(Tok::Ge) => BinOp::Ge,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_additive()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_unary()?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }
    /// Enter one nesting level, erroring past [`MAX_EXPR_DEPTH`] (M1 — the parser recurses
    /// per unary op and per paren group; an unbounded input would overflow the stack).
    fn descend(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            return Err(format!(
                "expression nests deeper than the {MAX_EXPR_DEPTH}-level limit"
            ));
        }
        Ok(())
    }
    fn ascend(&mut self) {
        self.depth -= 1;
    }
    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.eat(&Tok::Minus) {
            self.descend()?;
            let inner = self.parse_unary()?;
            self.ascend();
            return Ok(Expr::Neg(Box::new(inner)));
        }
        if self.eat(&Tok::Not) {
            self.descend()?;
            let inner = self.parse_unary()?;
            self.ascend();
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.parse_primary()
    }
    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Some(Tok::Num(s)) => Ok(Expr::Num(s)),
            Some(Tok::Ident(name)) => Ok(match name.as_str() {
                "true" => Expr::Bool(true),
                "false" => Expr::Bool(false),
                _ => Expr::Ref(name),
            }),
            Some(Tok::LParen) => {
                self.descend()?;
                let e = self.parse_or()?;
                self.ascend();
                if !self.eat(&Tok::RParen) {
                    return Err("expected `)`".into());
                }
                Ok(e)
            }
            Some(t) => Err(format!("unexpected token {t:?} in expression")),
            None => Err("unexpected end of expression".into()),
        }
    }
}

/// Parse an expression string into an [`Expr`] AST. A syntax error (unbalanced parens,
/// stray operator, trailing tokens) is an `E_EXPR`-class `Err`.
pub fn parse(s: &str) -> Result<Expr, String> {
    let toks = tokenize(s)?;
    if toks.is_empty() {
        return Err("empty expression".into());
    }
    let mut p = Parser {
        toks,
        pos: 0,
        depth: 0,
    };
    let e = p.parse_or()?;
    if p.pos != p.toks.len() {
        return Err(format!(
            "unexpected trailing tokens after expression (at token {})",
            p.pos
        ));
    }
    Ok(e)
}

// ----------------------------------------------------------------------------
// Evaluation
// ----------------------------------------------------------------------------

/// Evaluate an [`Expr`] against a parameter environment. Pure and terminating (the AST
/// has no loop or call node) — and **stack-safe**: it recurses per nesting level and is
/// depth-bounded ([`MAX_EXPR_DEPTH`]) so a hand-constructed deep tree (bypassing
/// [`parse`], which already caps depth) degrades to an `E_EXPR` error rather than an
/// abort. Every failure — unknown param, type mismatch, inexact division, overflow,
/// over-depth — is an `E_EXPR`-class `Err(String)`; the caller wraps it in a house
/// [`Diagnostic`](crate::diagnostic::Diagnostic).
pub fn eval(e: &Expr, env: &Env) -> Result<Value, String> {
    eval_d(e, env, 0)
}

fn eval_d(e: &Expr, env: &Env, depth: u32) -> Result<Value, String> {
    if depth > MAX_EXPR_DEPTH {
        return Err(format!(
            "expression nests deeper than the {MAX_EXPR_DEPTH}-level limit"
        ));
    }
    match e {
        Expr::Num(s) => eval_literal(s),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Ref(name) => env
            .get(name)
            .copied()
            .ok_or_else(|| format!("unknown param `{name}`")),
        Expr::Neg(inner) => match eval_d(inner, env, depth + 1)? {
            Value::Int(n) => n
                .checked_neg()
                .map(Value::Int)
                .ok_or_else(|| "integer overflow negating".to_string()),
            Value::Quantity(q) => q
                .mant
                .checked_neg()
                .map(|mant| Value::Quantity(Quantity { mant, exp: q.exp }))
                .ok_or_else(|| "overflow negating quantity".to_string()),
            Value::Bool(_) => Err("cannot negate a boolean (use `!`)".into()),
        },
        Expr::Not(inner) => Ok(Value::Bool(!eval_d(inner, env, depth + 1)?.as_bool()?)),
        Expr::Bin(op, l, r) => {
            let lv = eval_d(l, env, depth + 1)?;
            let rv = eval_d(r, env, depth + 1)?;
            eval_bin(*op, lv, rv)
        }
    }
}

/// Interpret a numeric literal token. A bare integer (no dot, no scale letter, no unit)
/// is an [`Int`](Value::Int) — so range bounds and counts stay honest integers; anything
/// with a fraction, SI multiplier, IEC letter, or unit is a [`Quantity`](Value::Quantity)
/// via the shared decimal-exact [`parse_quantity`]. A literal `quantity::parse` rejects
/// is an `E_EXPR` error.
fn eval_literal(s: &str) -> Result<Value, String> {
    // A pure `[0-9]+` (optionally already sign-free — the lexer never captures a sign
    // into a Num) is an integer.
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return s
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| format!("integer literal `{s}` overflows i64"));
    }
    match parse_quantity(s) {
        Some(q) => Ok(Value::Quantity(normalize(q))),
        None => Err(format!("`{s}` is not a valid number")),
    }
}

/// A binary operator on two evaluated values. Arithmetic promotes an [`Int`](Value::Int)
/// to a [`Quantity`](Value::Quantity) when the other side is a quantity (mixed
/// int/quantity math is allowed and exact); boolean operators demand booleans;
/// comparisons demand two numbers of a comparable kind. Every arithmetic path is
/// overflow-checked and division is exact-or-rejected.
fn eval_bin(op: BinOp, l: Value, r: Value) -> Result<Value, String> {
    use BinOp::*;
    match op {
        And | Or => {
            let (lb, rb) = (l.as_bool()?, r.as_bool()?);
            Ok(Value::Bool(match op {
                And => lb && rb,
                Or => lb || rb,
                _ => unreachable!(),
            }))
        }
        Eq | Ne => {
            let eq = values_equal(l, r)?;
            Ok(Value::Bool(if op == Eq { eq } else { !eq }))
        }
        Lt | Le | Gt | Ge => {
            let ord = compare(l, r)?;
            Ok(Value::Bool(match op {
                Lt => ord == std::cmp::Ordering::Less,
                Le => ord != std::cmp::Ordering::Greater,
                Gt => ord == std::cmp::Ordering::Greater,
                Ge => ord != std::cmp::Ordering::Less,
                _ => unreachable!(),
            }))
        }
        Add | Sub | Mul | Div => arith(op, l, r),
    }
}

/// `+ - * /` over int/quantity operands. Two integers stay integer (except an inexact
/// integer `/`, which promotes-and-rejects rather than truncating); any quantity operand
/// makes the result a quantity. Boolean operands are a type error.
fn arith(op: BinOp, l: Value, r: Value) -> Result<Value, String> {
    match (l, r) {
        (Value::Bool(_), _) | (_, Value::Bool(_)) => Err(format!(
            "cannot apply arithmetic to a boolean ({} {} {})",
            l.type_name(),
            op_symbol(op),
            r.type_name()
        )),
        (Value::Int(a), Value::Int(b)) => int_arith(op, a, b),
        _ => {
            // At least one quantity: promote both to quantities and do exact decimal math.
            let qa = to_quantity(l);
            let qb = to_quantity(r);
            quantity_arith(op, qa, qb).map(|q| Value::Quantity(normalize(q)))
        }
    }
}

/// Integer arithmetic with checked overflow. Division is exact-or-rejected: `10 / 5 = 2`
/// but `10 / 4` errors (2.5 is not an integer) — the author writes `2.5` if that is meant.
fn int_arith(op: BinOp, a: i64, b: i64) -> Result<Value, String> {
    use BinOp::*;
    Ok(Value::Int(match op {
        Add => a.checked_add(b).ok_or("integer overflow in `+`")?,
        Sub => a.checked_sub(b).ok_or("integer overflow in `-`")?,
        Mul => a.checked_mul(b).ok_or("integer overflow in `*`")?,
        Div => {
            if b == 0 {
                return Err("division by zero".into());
            }
            if a % b != 0 {
                return Err(format!(
                    "`{a} / {b}` is not an exact integer ({a} is not divisible by {b}); \
                     write the decimal value you mean (no silent rounding)"
                ));
            }
            a / b
        }
        _ => unreachable!("non-arith op routed to int_arith"),
    }))
}

/// Coerce a value known to be Int or Quantity into a [`Quantity`]. (Callers guarantee it
/// is not a Bool.)
fn to_quantity(v: Value) -> Quantity {
    match v {
        Value::Int(n) => Quantity { mant: n, exp: 0 },
        Value::Quantity(q) => q,
        Value::Bool(_) => unreachable!("bool guarded before to_quantity"),
    }
}

/// Decimal-exact quantity arithmetic. Add/sub align to the finer exponent (exact); mul
/// adds exponents and multiplies mantissas (exact); div is exact-or-rejected — the
/// quotient must be a terminating decimal `mant × 10^exp`, else `E_EXPR`. All checked.
fn quantity_arith(op: BinOp, a: Quantity, b: Quantity) -> Result<Quantity, String> {
    use BinOp::*;
    match op {
        Add | Sub => {
            let (ma, mb, e) = align(a, b).ok_or("overflow aligning quantities")?;
            let mant = if op == Add {
                ma.checked_add(mb).ok_or("overflow in `+`")?
            } else {
                ma.checked_sub(mb).ok_or("overflow in `-`")?
            };
            Ok(Quantity { mant, exp: e })
        }
        Mul => {
            let mant = a.mant.checked_mul(b.mant).ok_or("overflow in `*`")?;
            let exp = a.exp.checked_add(b.exp).ok_or("exponent overflow in `*`")?;
            Ok(Quantity { mant, exp })
        }
        Div => quantity_div(a, b),
        _ => unreachable!("non-arith op routed to quantity_arith"),
    }
}

/// Exact quantity division: `a / b` is representable as `mant × 10^exp` iff, after
/// factoring, the denominator's mantissa divides the numerator's cleanly once the
/// numerator has been scaled by a bounded power of ten. We reject anything not
/// terminating (e.g. `1 / 3`) rather than round. Zero denominator errors.
fn quantity_div(a: Quantity, b: Quantity) -> Result<Quantity, String> {
    let a = normalize(a);
    let b = normalize(b);
    if b.mant == 0 {
        return Err("division by zero".into());
    }
    if a.mant == 0 {
        return Ok(Quantity { mant: 0, exp: 0 });
    }
    // We want mant_q, exp_q with a.mant / b.mant × 10^(a.exp - b.exp) = mant_q × 10^exp_q.
    // Scale the numerator by 10^k (increasing digits, decreasing exp) until b.mant | it.
    // A terminating decimal exists iff b.mant's prime factors are only 2s and 5s after
    // dividing out the gcd — we discover that by trying to clear the denominator with a
    // bounded k; if k exceeds the i64 digit budget without clearing, it is non-terminating.
    let base_exp = (a.exp as i64) - (b.exp as i64);
    let mut num = a.mant;
    let mut k: i32 = 0;
    // 18 powers of ten is the i64 headroom; a repeating decimal never clears, so this
    // bound distinguishes "terminating but long" from "non-terminating" without looping
    // unboundedly.
    while num % b.mant != 0 {
        num = num
            .checked_mul(10)
            .ok_or("quantity division does not terminate as an exact decimal")?;
        k += 1;
        if k > 18 {
            return Err("quantity division does not terminate as an exact decimal".into());
        }
    }
    let mant = num / b.mant;
    let exp = i32::try_from(base_exp - k as i64).map_err(|_| "exponent overflow in `/`")?;
    Ok(normalize(Quantity { mant, exp }))
}

/// Structural equality for `==`/`!=`. Numbers (int/quantity) compare by value after
/// alignment; a boolean compares only with a boolean. Cross-kind number comparison
/// (int vs quantity) is allowed and exact; a number vs a boolean is a type error.
fn values_equal(l: Value, r: Value) -> Result<bool, String> {
    match (l, r) {
        (Value::Bool(a), Value::Bool(b)) => Ok(a == b),
        (Value::Bool(_), _) | (_, Value::Bool(_)) => Err(format!(
            "cannot compare {} with {}",
            l.type_name(),
            r.type_name()
        )),
        _ => Ok(compare(l, r)? == std::cmp::Ordering::Equal),
    }
}

/// Ordering for `< <= > >=` (and value equality). Both operands must be numbers; the
/// comparison is exact via a common-exponent alignment. Booleans are unordered (a type
/// error).
fn compare(l: Value, r: Value) -> Result<std::cmp::Ordering, String> {
    let (a, b) = match (l, r) {
        (Value::Bool(_), _) | (_, Value::Bool(_)) => {
            return Err(format!(
                "cannot compare {} with {}",
                l.type_name(),
                r.type_name()
            ));
        }
        _ => (to_quantity(l), to_quantity(r)),
    };
    let (ma, mb, _) = align(a, b).ok_or("overflow comparing quantities")?;
    Ok(ma.cmp(&mb))
}

fn op_symbol(op: BinOp) -> &'static str {
    use BinOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        Eq => "==",
        Ne => "!=",
        And => "&&",
        Or => "||",
    }
}

// ----------------------------------------------------------------------------
// Param resolution (cycle-safe)
// ----------------------------------------------------------------------------

/// Resolve a set of `param name = expr` declarations into a fully-evaluated [`Env`],
/// detecting cycles. Params may reference earlier-or-later params (order-independent),
/// so this is a DFS with a visiting set; a reference chain that loops back on itself is
/// an `E_EXPR` cycle error naming the cycle. A duplicate param name is an error (the
/// caller passes a deduped map, so this is belt-and-suspenders on the map contract).
///
/// `decls` maps param name → its authored expression text. Returns the resolved
/// environment or the first structural error encountered (parse error, unknown ref,
/// type/eval error, or cycle) — expression errors are one-at-a-time by nature (a bad
/// param poisons everything downstream), unlike the collect-all directive passes.
pub fn resolve_params(decls: &BTreeMap<String, String>) -> Result<Env, String> {
    // Pre-parse every declaration once (a parse error surfaces here with its name).
    let mut asts: BTreeMap<String, Expr> = BTreeMap::new();
    for (name, text) in decls {
        let ast = parse(text).map_err(|e| format!("param `{name}`: {e}"))?;
        asts.insert(name.clone(), ast);
    }
    let mut env: Env = BTreeMap::new();
    let mut visiting: Vec<String> = Vec::new();
    for name in decls.keys() {
        resolve_one(name, &asts, &mut env, &mut visiting)?;
    }
    Ok(env)
}

/// Resolve one param (and, recursively, its dependencies) into `env`. `visiting` is the
/// active DFS stack; re-entering a name already on it is a cycle.
fn resolve_one(
    name: &str,
    asts: &BTreeMap<String, Expr>,
    env: &mut Env,
    visiting: &mut Vec<String>,
) -> Result<Value, String> {
    if let Some(v) = env.get(name) {
        return Ok(*v);
    }
    if visiting.iter().any(|n| n == name) {
        visiting.push(name.to_string());
        return Err(format!("param cycle: {} → {name}", visiting.join(" → ")));
    }
    let ast = asts
        .get(name)
        .ok_or_else(|| format!("unknown param `{name}`"))?;
    visiting.push(name.to_string());
    // Collect referenced names and resolve them first, then evaluate against `env`.
    let mut refs = Vec::new();
    collect_refs(ast, &mut refs);
    for r in refs {
        // A reference to a name with no declaration is caught by `eval` below; only
        // resolve names we actually have declarations for (so unknown-param wins the
        // clearer message from `eval`).
        if asts.contains_key(&r) && !env.contains_key(&r) {
            resolve_one(&r, asts, env, visiting)?;
        }
    }
    let v = eval(ast, env).map_err(|e| format!("param `{name}`: {e}"))?;
    visiting.pop();
    env.insert(name.to_string(), v);
    Ok(v)
}

/// Collect every param name referenced by an expression (for dependency ordering).
/// Depth-bounded like [`eval`]: it recurses on nesting, so it stops descending past
/// [`MAX_EXPR_DEPTH`] rather than risking a stack overflow on a hand-built deep tree —
/// an over-deep expression is caught with an `E_EXPR` error by the [`eval`] that follows
/// this collection in [`resolve_one`] (and ASTs from [`parse`] are already depth-capped).
fn collect_refs(e: &Expr, out: &mut Vec<String>) {
    collect_refs_d(e, out, 0)
}

fn collect_refs_d(e: &Expr, out: &mut Vec<String>, depth: u32) {
    if depth > MAX_EXPR_DEPTH {
        return;
    }
    match e {
        Expr::Ref(n) => out.push(n.clone()),
        Expr::Num(_) | Expr::Bool(_) => {}
        Expr::Neg(i) | Expr::Not(i) => collect_refs_d(i, out, depth + 1),
        Expr::Bin(_, l, r) => {
            collect_refs_d(l, out, depth + 1);
            collect_refs_d(r, out, depth + 1);
        }
    }
}

/// Evaluate a single expression string against a resolved environment — the entry point
/// consumers (`p:` values, range bounds, `if=`) use. Parses then evaluates; an `E_EXPR`
/// error carries the reason.
pub fn eval_str(s: &str, env: &Env) -> Result<Value, String> {
    eval(&parse(s)?, env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> Env {
        let decls: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        resolve_params(&decls).unwrap()
    }

    fn ev(s: &str, env: &Env) -> Value {
        eval_str(s, env).unwrap()
    }

    #[test]
    fn integer_arithmetic_is_exact() {
        let e = Env::new();
        assert_eq!(ev("1 + 2 * 3", &e), Value::Int(7));
        assert_eq!(ev("(1 + 2) * 3", &e), Value::Int(9));
        assert_eq!(ev("10 - 3 - 2", &e), Value::Int(5));
        assert_eq!(ev("10 / 5", &e), Value::Int(2));
        assert_eq!(ev("-4 + 1", &e), Value::Int(-3));
    }

    #[test]
    fn inexact_integer_division_is_rejected() {
        let e = Env::new();
        assert!(eval_str("10 / 4", &e).is_err());
        assert!(eval_str("1 / 3", &e).is_err());
        assert!(eval_str("10 / 0", &e).is_err());
    }

    #[test]
    fn quantity_arithmetic_is_decimal_exact() {
        let e = Env::new();
        // 4.7k + 300 = 5000
        assert_eq!(
            ev("4.7k + 300", &e),
            Value::Quantity(normalize(Quantity { mant: 5, exp: 3 }))
        );
        // 2.5 * 4 = 10 (quantity, since 2.5 is a quantity); normalizes to 1×10^1.
        assert_eq!(
            ev("2.5 * 4", &e),
            Value::Quantity(normalize(Quantity { mant: 10, exp: 0 }))
        );
        // exact division: 1 / 4 = 0.25
        assert_eq!(
            ev("1.0 / 4", &e),
            Value::Quantity(Quantity { mant: 25, exp: -2 })
        );
        // 100nF stripped of unit parses as a quantity
        assert_eq!(
            ev("100nF * 2", &e),
            Value::Quantity(normalize(Quantity { mant: 200, exp: -9 }))
        );
    }

    #[test]
    fn inexact_quantity_division_is_rejected() {
        let e = Env::new();
        assert!(eval_str("1.0 / 3", &e).is_err()); // 0.333... not terminating
        assert!(eval_str("10.0 / 3", &e).is_err());
    }

    #[test]
    fn comparisons_and_booleans() {
        let e = env_of(&[("n", "3")]);
        assert_eq!(ev("n > 2", &e), Value::Bool(true));
        assert_eq!(ev("n >= 3 && n < 10", &e), Value::Bool(true));
        assert_eq!(ev("n == 4 || n == 3", &e), Value::Bool(true));
        assert_eq!(ev("!(n == 3)", &e), Value::Bool(false));
        assert_eq!(ev("4.7k > 4000", &e), Value::Bool(true));
        assert_eq!(ev("true && false", &e), Value::Bool(false));
    }

    #[test]
    fn param_references_resolve_across_order() {
        // b defined before a but references a — order-independent.
        let e = env_of(&[("b", "a + 1"), ("a", "10")]);
        assert_eq!(e["a"], Value::Int(10));
        assert_eq!(e["b"], Value::Int(11));
    }

    #[test]
    fn param_cycle_is_rejected() {
        let decls: BTreeMap<String, String> = [("a", "b + 1"), ("b", "a + 1")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let err = resolve_params(&decls).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn unknown_param_is_rejected() {
        let e = Env::new();
        assert!(eval_str("missing + 1", &e).is_err());
    }

    #[test]
    fn type_mismatches_are_rejected() {
        let e = env_of(&[("n", "3"), ("flag", "true")]);
        assert!(eval_str("flag + 1", &e).is_err()); // bool + int
        assert!(eval_str("n && n", &e).is_err()); // int as bool
        assert!(eval_str("n > flag", &e).is_err()); // compare int/bool
        assert!(eval_str("flag == 1", &e).is_err()); // eq across kinds
    }

    #[test]
    fn overflow_is_reported_not_panicked() {
        let e = Env::new();
        assert!(eval_str("9223372036854775807 + 1", &e).is_err());
        assert!(eval_str("9223372036854775807 * 2", &e).is_err());
    }

    #[test]
    fn deep_nesting_errors_instead_of_overflowing_the_stack() {
        // At the bound: MAX_EXPR_DEPTH paren levels around a literal parses+evals fine.
        let at = format!(
            "{}1{}",
            "(".repeat(MAX_EXPR_DEPTH as usize),
            ")".repeat(MAX_EXPR_DEPTH as usize)
        );
        assert_eq!(eval_str(&at, &Env::new()).unwrap(), Value::Int(1));
        // Just past the bound: an `E_EXPR` error, NOT a process abort (M1).
        let over = format!(
            "{}1{}",
            "(".repeat(MAX_EXPR_DEPTH as usize + 1),
            ")".repeat(MAX_EXPR_DEPTH as usize + 1)
        );
        let err = eval_str(&over, &Env::new()).unwrap_err();
        assert!(err.contains("nests deeper"), "got: {err}");
        // A deep unary chain is bounded too (the other parser recursion point).
        let unary = format!("{}1", "-".repeat(MAX_EXPR_DEPTH as usize + 5));
        assert!(eval_str(&unary, &Env::new()).is_err());
    }

    #[test]
    fn as_index_and_as_bool_coercions() {
        assert_eq!(Value::Int(5).as_index().unwrap(), 5);
        assert_eq!(
            Value::Quantity(Quantity { mant: 4, exp: 3 })
                .as_index()
                .unwrap(),
            4000
        );
        assert!(
            Value::Quantity(Quantity { mant: 25, exp: -2 })
                .as_index()
                .is_err()
        );
        assert!(Value::Bool(true).as_index().is_err());
        assert!(Value::Int(1).as_bool().is_err());
        assert!(Value::Bool(true).as_bool().unwrap());
    }
}
