// Copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

use crate::Expr;
use tokenizer::presets::default_pipeline;

use super::lower::LowerError;
use super::subtokenize::SubTokenizeError;

#[derive(Debug, Clone, PartialEq)]
pub enum CapabilityClass {
    SemanticSupported,
    ExpectedLowerError(LoweringIssue),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoweringIssue {
    Parse,
    EmptyRangeBound,
    SplitLongToken,
    MatchAllInSpanContext,
    InvalidRegex,
    /// Past a size or nesting limit (see [`crate::limits`]).
    TooLarge,
}

pub fn classify_expr<T>(expr: &Expr, tokenizer: &T) -> CapabilityClass
where
    T: tokenizer::Tokenizer,
{
    let expr = match super::subtokenize::sub_tokenize(expr.clone(), tokenizer) {
        Ok(expr) => expr,
        Err(err) => return CapabilityClass::ExpectedLowerError(LoweringIssue::from(err)),
    };

    match super::lower::lower(&expr) {
        Ok(_) => CapabilityClass::SemanticSupported,
        Err(err) => CapabilityClass::ExpectedLowerError(LoweringIssue::from(err)),
    }
}

pub fn classify_query_text(query_text: &str) -> CapabilityClass {
    match crate::parse(query_text, crate::ImplicitOp::And) {
        Ok(expr) => classify_expr(&expr, default_pipeline()),
        Err(_) => CapabilityClass::ExpectedLowerError(LoweringIssue::Parse),
    }
}

impl From<SubTokenizeError> for LoweringIssue {
    fn from(value: SubTokenizeError) -> Self {
        match value {
            SubTokenizeError::EmptyRangeBound { .. } => Self::EmptyRangeBound,
            SubTokenizeError::SplitWildcardLiteral { .. }
            | SubTokenizeError::SplitFuzzyTerm { .. }
            | SubTokenizeError::SplitRangeBound { .. } => Self::SplitLongToken,
            SubTokenizeError::TooManyTerms => Self::TooLarge,
        }
    }
}

impl From<LowerError> for LoweringIssue {
    fn from(value: LowerError) -> Self {
        match value {
            LowerError::MatchAllInSpanContext => Self::MatchAllInSpanContext,
            LowerError::InvalidRegex(_) => Self::InvalidRegex,
            LowerError::NestingTooDeep => Self::TooLarge,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImplicitOp;
    use tokenizer::presets::default_pipeline;

    fn parse(input: &str) -> Expr {
        crate::parse(input, ImplicitOp::And).expect("query should parse")
    }

    #[test]
    fn classifies_supported_query() {
        assert_eq!(
            classify_expr(&parse("\"craft beer\" THEN/2 hops"), default_pipeline()),
            CapabilityClass::SemanticSupported
        );
    }

    #[test]
    fn classifies_expansion_queries_as_supported() {
        assert_eq!(
            classify_expr(&parse("brew*"), default_pipeline()),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(&parse("MATCHES hop.*s"), default_pipeline()),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(&parse("aardvark TO cat"), default_pipeline()),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(&parse("beer~2"), default_pipeline()),
            CapabilityClass::SemanticSupported
        );
    }

    #[test]
    fn classifies_expected_lower_errors() {
        assert_eq!(
            classify_expr(&parse("* THEN/0 beer"), default_pipeline()),
            CapabilityClass::ExpectedLowerError(LoweringIssue::MatchAllInSpanContext)
        );
        assert_eq!(
            classify_expr(&Expr::Regex("(".into()), default_pipeline()),
            CapabilityClass::ExpectedLowerError(LoweringIssue::InvalidRegex)
        );
    }

    #[test]
    fn classifies_runtime_span_features_as_supported() {
        assert_eq!(
            classify_expr(&parse("beer IN FIRST 25%"), default_pipeline()),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(&parse("beer IN LAST 10 WORDS"), default_pipeline()),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(&parse("beer IN MIDDLE 50%"), default_pipeline()),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(
                &parse("(beer IN FIRST 25%) BEFORE wine"),
                default_pipeline()
            ),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(
                &parse("(beer IN LAST 10 WORDS) BEFORE wine"),
                default_pipeline()
            ),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(
                &parse("(beer IN MIDDLE 50%) BEFORE wine"),
                default_pipeline()
            ),
            CapabilityClass::SemanticSupported
        );
        assert_eq!(
            classify_expr(
                &parse("AT LEAST 1 OF [beer, wine] THEN/0 hops"),
                default_pipeline()
            ),
            CapabilityClass::SemanticSupported
        );
    }
}
