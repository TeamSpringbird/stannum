pub mod ast;
pub mod error;
mod parser;
mod quote;
pub mod runtime;
mod util;

#[cfg(test)]
mod tests;

pub use ast::*;
pub use error::ParseError;
pub use quote::maybe_quote;

/// The operator inserted between adjacent expressions that lack an
/// explicit boolean keyword.
///
/// - `And` — `beer wine` → `beer AND wine` (Google-style)
/// - `Or` — `beer wine` → `beer OR wine` (Lucene default-style)
#[derive(Clone, Copy)]
pub enum ImplicitOp {
    And,
    Or,
}

/// Parse a query string into an expression tree.
///
/// `implicit_op` controls how adjacent expressions without an explicit
/// operator are joined: `beer wine` becomes `beer AND wine` with
/// [`ImplicitOp::And`], or `beer OR wine` with [`ImplicitOp::Or`].
///
/// # Errors
///
/// Returns [`ParseError`] if the input is not a valid query string.
pub fn parse(input: &str, implicit_op: ImplicitOp) -> Result<Expr, ParseError> {
    if input.trim().is_empty() {
        // An empty query matches nothing; it is not a syntax error.
        return Ok(Expr::MatchNone);
    }
    parser::pest_parser::parse(input, implicit_op)
}
