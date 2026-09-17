#[derive(Debug, thiserror::Error)]
pub enum SpanError {
    #[error("term index {index} out of range (num_terms = {num_terms})")]
    TermIndexOutOfRange { index: usize, num_terms: usize },

    #[error("operator requires at least {min} children, got {got}")]
    TooFewChildren { min: usize, got: usize },

    #[error("empty query")]
    EmptyQuery,
}
