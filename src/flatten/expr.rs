//! Evaluation of aperture-macro arithmetic (Gerber spec §4.5.4).
//!
//! Expressions use decimal constants, variables `$n`, the operators `+`, `-`,
//! `x` (multiply) and `/`, unary minus and parentheses, with the usual
//! precedence (`x` and `/` bind tighter than `+` and `-`).

use std::collections::HashMap;

/// Deepest parenthesis / unary nesting accepted, so a hostile file cannot
/// overflow the stack.
pub const MAX_EXPRESSION_DEPTH: usize = 256;

/// Why an expression could not be evaluated.
#[derive(thiserror::Error, Debug, Clone, PartialEq)]
pub enum ExprError {
    /// The text is not a valid expression; `at` is the byte offset.
    #[error("invalid macro expression '{expr}' at position {at}")]
    Syntax { expr: String, at: usize },
    /// Nesting deeper than [`MAX_EXPRESSION_DEPTH`].
    #[error("macro expression '{expr}' is nested deeper than {MAX_EXPRESSION_DEPTH} levels")]
    TooDeep { expr: String },
    /// The result is NaN or infinite (e.g. division by zero).
    #[error("macro expression '{expr}' has no finite value (division by zero?)")]
    NotFinite { expr: String },
}

/// Macro variables `$1`, `$2`, … in scope. Undefined variables read as 0,
/// as the spec requires for parameters the aperture definition omits.
#[derive(Debug, Clone, Default)]
pub struct Variables(HashMap<u32, f64>);

impl Variables {
    /// Variables from aperture-definition arguments: `args[0]` is `$1`.
    pub fn from_args(args: &[f64]) -> Self {
        Self(
            args.iter()
                .enumerate()
                .map(|(i, v)| (i as u32 + 1, *v))
                .collect(),
        )
    }

    pub fn get(&self, n: u32) -> f64 {
        self.0.get(&n).copied().unwrap_or(0.0)
    }

    pub fn set(&mut self, n: u32, value: f64) {
        self.0.insert(n, value);
    }
}

/// Evaluates `expr` with the given variables.
pub fn eval(expr: &str, vars: &Variables) -> Result<f64, ExprError> {
    let mut p = Parser {
        src: expr.as_bytes(),
        pos: 0,
        depth: 0,
        vars,
        expr,
    };
    let value = p.sum()?;
    p.skip_ws();
    if p.pos != p.src.len() {
        return Err(p.syntax());
    }
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ExprError::NotFinite {
            expr: expr.to_string(),
        })
    }
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    depth: usize,
    vars: &'a Variables,
    expr: &'a str,
}

impl Parser<'_> {
    fn syntax(&self) -> ExprError {
        ExprError::Syntax {
            expr: self.expr.to_string(),
            at: self.pos,
        }
    }

    fn skip_ws(&mut self) {
        while self.src.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.src.get(self.pos).copied()
    }

    fn enter(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_EXPRESSION_DEPTH {
            return Err(ExprError::TooDeep {
                expr: self.expr.to_string(),
            });
        }
        Ok(())
    }

    /// sum := product (('+' | '-') product)*
    fn sum(&mut self) -> Result<f64, ExprError> {
        let mut value = self.product()?;
        while let Some(op @ (b'+' | b'-')) = self.peek() {
            self.pos += 1;
            let rhs = self.product()?;
            value = if op == b'+' { value + rhs } else { value - rhs };
        }
        Ok(value)
    }

    /// product := unary (('x' | 'X' | '/') unary)*
    fn product(&mut self) -> Result<f64, ExprError> {
        let mut value = self.unary()?;
        while let Some(op @ (b'x' | b'X' | b'/')) = self.peek() {
            self.pos += 1;
            let rhs = self.unary()?;
            value = if op == b'/' { value / rhs } else { value * rhs };
        }
        Ok(value)
    }

    /// unary := ('-' | '+') unary | atom
    fn unary(&mut self) -> Result<f64, ExprError> {
        match self.peek() {
            Some(sign @ (b'-' | b'+')) => {
                self.pos += 1;
                self.enter()?;
                let value = self.unary()?;
                self.depth -= 1;
                Ok(if sign == b'-' { -value } else { value })
            }
            _ => self.atom(),
        }
    }

    /// atom := number | '$' digits | '(' sum ')'
    fn atom(&mut self) -> Result<f64, ExprError> {
        match self.peek() {
            Some(b'(') => {
                self.pos += 1;
                self.enter()?;
                let value = self.sum()?;
                self.depth -= 1;
                if self.peek() != Some(b')') {
                    return Err(self.syntax());
                }
                self.pos += 1;
                Ok(value)
            }
            Some(b'$') => {
                self.pos += 1;
                let start = self.pos;
                while self.src.get(self.pos).is_some_and(u8::is_ascii_digit) {
                    self.pos += 1;
                }
                std::str::from_utf8(&self.src[start..self.pos])
                    .ok()
                    .and_then(|s| s.parse::<u32>().ok())
                    .map(|n| self.vars.get(n))
                    .ok_or_else(|| self.syntax())
            }
            Some(c) if c.is_ascii_digit() || c == b'.' => {
                let start = self.pos;
                while self
                    .src
                    .get(self.pos)
                    .is_some_and(|c| c.is_ascii_digit() || *c == b'.')
                {
                    self.pos += 1;
                }
                std::str::from_utf8(&self.src[start..self.pos])
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| self.syntax())
            }
            _ => Err(self.syntax()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(expr: &str, args: &[f64]) -> f64 {
        eval(expr, &Variables::from_args(args)).unwrap()
    }

    #[test]
    fn precedence_and_parentheses() {
        assert_eq!(ev("1+2x3", &[]), 7.0);
        assert_eq!(ev("(1+2)x3", &[]), 9.0);
        assert_eq!(ev("8/2/2", &[]), 2.0);
        assert_eq!(ev("10-2-3", &[]), 5.0);
    }

    #[test]
    fn variables_and_unary_minus() {
        assert_eq!(ev("$1x25.4", &[2.0]), 50.8);
        assert_eq!(ev("-$1", &[3.0]), -3.0);
        assert_eq!(ev("$3-$1/2", &[4.0, 0.0, 0.5]), -1.5);
        // Undefined variables read as 0.
        assert_eq!(ev("$9+1", &[]), 1.0);
    }

    #[test]
    fn errors() {
        let v = Variables::default();
        assert!(matches!(eval("1/0", &v), Err(ExprError::NotFinite { .. })));
        assert!(matches!(eval("1+", &v), Err(ExprError::Syntax { .. })));
        assert!(matches!(eval("(1", &v), Err(ExprError::Syntax { .. })));
        let deep = format!("{}1{}", "(".repeat(1000), ")".repeat(1000));
        assert!(matches!(eval(&deep, &v), Err(ExprError::TooDeep { .. })));
    }
}
