//! Sift: small programs an agent writes to answer a data question in one run
//! instead of many tool rounds.
//!
//! A program is a sequence of `let`, `if`, `for` and `return` statements over
//! immutable values: null, bools, 64-bit ints, floats, strings, lists and
//! records. Tools are called by their model-facing names with one record of
//! arguments; `sql(db) { ... }` runs a query whose `${...}` holes become
//! parameters, never spliced text; `all(...)` runs calls concurrently. The
//! library (`one`, `group_by`, `join`, `xs.map`, …) needs nothing from the
//! host.
//!
//! [`run`] lexes, parses and checks the program, then interprets it against a
//! [`Host`] within [`Limits`]. The [`Outcome`] is the return value, the
//! diagnostics that rejected the program before anything ran, or the error
//! that stopped it; [`Outcome::render`] is the text the model reads.
//!
//! Choices the language leaves open: `1 == 1.0` (numbers compare by value);
//! `/` always divides to a float, `%` of two ints stays an int, and dividing
//! by zero is an error; `int(3.7)` truncates to 3; `r?.field` is null when `r` is null or has no
//! such field.

mod ast;
mod check;
mod diagnostic;
mod host;
mod interp;
mod lexer;
mod library;
mod outcome;
mod parser;
mod suggest;
mod value;

pub use diagnostic::{code, Diagnostic};
pub use host::{Host, Limits};
pub use outcome::{JournalEntry, Outcome, Stats};

use std::sync::Arc;

/// Parse, check and run one program.
pub async fn run(source: &str, host: Arc<dyn Host>, limits: Limits) -> Outcome {
    let program = match parser::parse(source) {
        Ok(program) => program,
        Err(diagnostics) => return Outcome::Rejected { diagnostics },
    };
    let interp = interp::Interp::new(host, limits);
    let result = interp.run(&program).await;
    let stats = interp.stats();
    match result {
        Ok(value) => Outcome::Returned { value, stats },
        Err(error) => Outcome::Failed { error, stats },
    }
}

#[cfg(test)]
mod tests;
