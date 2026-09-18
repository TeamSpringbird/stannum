#[allow(unused_imports)]
use crate::am::amhandler;
#[allow(unused_imports)]
use crate::selectivity::stannum_text_restrict;
use pgrx::{extension_sql, pg_extern};
use tinql::runtime::{evaluate, lower::lower, subtokenize::sub_tokenize, tokenize_doc};
use tokenizer::presets::default_pipeline;

fn evaluate_text(document: &str, query_text: &str) -> Result<bool, String> {
    let pipeline = default_pipeline();
    let parsed = tinql::parse(query_text, tinql::ImplicitOp::And).map_err(|e| e.to_string())?;
    let analyzed = sub_tokenize(parsed, pipeline).map_err(|e| e.to_string())?;
    let query = lower(&analyzed).map_err(|e| e.to_string())?;
    let document = tokenize_doc(document, pipeline);
    evaluate(&query, &document)
        .map(|result| result.matched)
        .map_err(|e| e.to_string())
}

#[pg_extern(immutable, parallel_safe)]
pub fn stannum_text_cmpfunc(document: &str, query: &str) -> bool {
    evaluate_text(document, query)
        .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"))
}

extension_sql!(
    r#"
CREATE OPERATOR pg_catalog.==> (
    PROCEDURE = @extschema@.stannum_text_cmpfunc,
    LEFTARG = pg_catalog.text,
    RIGHTARG = pg_catalog.text,
    RESTRICT = @extschema@.stannum_text_restrict
);

CREATE OPERATOR CLASS @extschema@.stannum_text_ops DEFAULT FOR TYPE pg_catalog.text USING stannum AS
    OPERATOR 1 pg_catalog.==>(pg_catalog.text, pg_catalog.text),
    STORAGE pg_catalog.text;
"#,
    name = "stannum_text_operator",
    requires = [amhandler, stannum_text_cmpfunc, stannum_text_restrict]
);

#[cfg(test)]
mod tests {
    use super::evaluate_text;

    #[test]
    fn boolean_and_positional_queries_are_exact() {
        assert!(evaluate_text("A craft beer bar", "craft AND beer").unwrap());
        assert!(evaluate_text("A craft beer bar", "\"craft beer\"").unwrap());
        assert!(!evaluate_text("Beer for craft fans", "\"craft beer\"").unwrap());
    }

    #[test]
    fn expansions_use_the_document_term_universe() {
        assert!(evaluate_text("brewhouse", "brew*").unwrap());
        assert!(evaluate_text("jalapeno", "jalapeño~1").unwrap());
        assert!(!evaluate_text("winery", "brew*").unwrap());
    }

    #[test]
    fn empty_documents_do_not_match_match_all() {
        assert!(!evaluate_text("...", "*").unwrap());
    }

    #[test]
    fn invalid_queries_are_reported() {
        assert!(evaluate_text("beer", "beer OR").is_err());
    }
}
