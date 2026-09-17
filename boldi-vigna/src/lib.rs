mod display;
mod error;
mod interval;
mod positions;
mod query;
mod solver;
pub(crate) mod state;

pub use error::SpanError;
pub use interval::Interval;
pub use positions::TermPositions;
pub use query::SpanQuery;
pub use solver::{Intervals, SpanSolver};
