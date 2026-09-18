use pgrx::iter::TableIterator;
use pgrx::{PgRelation, default, iter::SetOfIterator, name, pg_extern};
use tokenizer::{
    Folding, GraphemeMode, LongTokenMode, LongTokenSpec, PositionGapMode, Tokenizer,
    TokenizerPipelineSpec, TokenizerSpec,
};

pub const MAX_TOKEN_BYTES: usize = 2_692;

#[derive(Debug, Clone, Copy)]
struct TokenizeOptions<'a> {
    tokenizer: &'a str,
    case_folding: &'a str,
    accent_folding: &'a str,
    long_tokens: &'a str,
    max_token_bytes: i32,
    graphemes: &'a str,
    position_gaps: &'a str,
}

impl TokenizeOptions<'_> {
    fn into_spec(self) -> Result<TokenizerPipelineSpec, String> {
        let max_bytes = usize::try_from(self.max_token_bytes)
            .ok()
            .filter(|n| (tokenizer::MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(n))
            .ok_or_else(|| {
                format!(
                    "max_token_bytes must be between {} and {MAX_TOKEN_BYTES}",
                    tokenizer::MIN_TOKEN_BYTES
                )
            })?;

        Ok(TokenizerPipelineSpec {
            tokenizer: parse_tokenizer(self.tokenizer)?,
            case_folding: parse_folding("case_folding", self.case_folding)?,
            accent_folding: parse_folding("accent_folding", self.accent_folding)?,
            long_tokens: LongTokenSpec {
                mode: parse_long_tokens(self.long_tokens)?,
                max_bytes,
            },
            graphemes: parse_graphemes(self.graphemes)?,
            position_gaps: parse_position_gaps(self.position_gaps)?,
        })
    }
}

fn parse_tokenizer(value: &str) -> Result<TokenizerSpec, String> {
    match value {
        "unicode" => Ok(TokenizerSpec::Unicode),
        "whitespace" => Ok(TokenizerSpec::Whitespace),
        _ => Err(format!(
            "invalid tokenizer value {value:?}; expected unicode or whitespace"
        )),
    }
}

fn parse_folding(option: &str, value: &str) -> Result<Folding, String> {
    match value {
        "preserve" => Ok(Folding::Preserve),
        "fold" => Ok(Folding::Fold),
        _ => Err(format!(
            "invalid {option} value {value:?}; expected preserve or fold"
        )),
    }
}

fn parse_long_tokens(value: &str) -> Result<LongTokenMode, String> {
    match value {
        "truncate" => Ok(LongTokenMode::Truncate),
        "discard" => Ok(LongTokenMode::Discard),
        "split" => Ok(LongTokenMode::Split),
        _ => Err(format!(
            "invalid long_tokens value {value:?}; expected truncate, discard, or split"
        )),
    }
}

fn parse_graphemes(value: &str) -> Result<GraphemeMode, String> {
    match value {
        "discard" => Ok(GraphemeMode::Discard),
        "emoji" => Ok(GraphemeMode::Emoji),
        "retain" => Ok(GraphemeMode::Retain),
        _ => Err(format!(
            "invalid graphemes value {value:?}; expected discard, emoji, or retain"
        )),
    }
}

fn parse_position_gaps(value: &str) -> Result<PositionGapMode, String> {
    match value {
        "collapse" => Ok(PositionGapMode::Collapse),
        "preserve" => Ok(PositionGapMode::Preserve),
        _ => Err(format!(
            "invalid position_gaps value {value:?}; expected collapse or preserve"
        )),
    }
}

fn compile_options(options: TokenizeOptions<'_>) -> tokenizer::CompiledTokenizerPipeline {
    options
        .into_spec()
        .and_then(|spec| spec.compile().map_err(|e| e.to_string()))
        .unwrap_or_else(|error| pgrx::error!("{error}"))
}

#[cfg(test)]
fn collect_tokens(text: &str, spec: TokenizerPipelineSpec) -> Vec<String> {
    spec.compile()
        .expect("validated tokenizer specification")
        .tokenize(text)
        .map(|token| token.text.into_owned())
        .collect()
}

#[pg_extern(immutable, parallel_safe)]
#[expect(clippy::too_many_arguments, reason = "TIN-compatible SQL signature")]
pub fn tokenize<'a>(
    text: Option<&'a str>,
    tokenizer: default!(&str, "'unicode'"),
    case_folding: default!(&str, "'fold'"),
    accent_folding: default!(&str, "'fold'"),
    long_tokens: default!(&str, "'split'"),
    max_token_bytes: default!(i32, 256),
    graphemes: default!(&str, "'emoji'"),
    position_gaps: default!(&str, "'preserve'"),
) -> SetOfIterator<'a, String> {
    let pipeline = compile_options(TokenizeOptions {
        tokenizer,
        case_folding,
        accent_folding,
        long_tokens,
        max_token_bytes,
        graphemes,
        position_gaps,
    });
    match text {
        Some(text) => SetOfIterator::new(
            pipeline
                .tokenize(text)
                .map(|token| token.text.into_owned())
                .collect::<Vec<_>>(),
        ),
        None => SetOfIterator::empty(),
    }
}

#[pg_extern(immutable, parallel_safe)]
pub fn maybe_quote(text: Option<&str>) -> Option<String> {
    text.map(|text| tinql::maybe_quote(text).into_owned())
}

#[pg_extern(immutable, parallel_safe)]
#[expect(clippy::too_many_arguments, reason = "TIN-compatible SQL signature")]
pub fn ql_parse(
    query: Option<&str>,
    surface: default!(bool, true),
    tokenizer: default!(&str, "'unicode'"),
    case_folding: default!(&str, "'fold'"),
    accent_folding: default!(&str, "'fold'"),
    long_tokens: default!(&str, "'split'"),
    max_token_bytes: default!(i32, 256),
    graphemes: default!(&str, "'emoji'"),
    position_gaps: default!(&str, "'preserve'"),
) -> Option<String> {
    let query = query?;
    let pipeline = compile_options(TokenizeOptions {
        tokenizer,
        case_folding,
        accent_folding,
        long_tokens,
        max_token_bytes,
        graphemes,
        position_gaps,
    });
    let parsed =
        tinql::parse(query, tinql::ImplicitOp::And).unwrap_or_else(|error| pgrx::error!("{error}"));
    let analyzed = tinql::runtime::subtokenize::sub_tokenize(parsed, &pipeline)
        .unwrap_or_else(|error| pgrx::error!("{error}"));
    if surface {
        Some(analyzed.to_string())
    } else {
        Some(
            tinql::runtime::lower::lower(&analyzed)
                .unwrap_or_else(|error| pgrx::error!("{error}"))
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_tokenizer_options() {
        let spec = TokenizeOptions {
            tokenizer: "whitespace",
            case_folding: "preserve",
            accent_folding: "preserve",
            long_tokens: "truncate",
            max_token_bytes: 32,
            graphemes: "retain",
            position_gaps: "collapse",
        }
        .into_spec()
        .unwrap();
        assert_eq!(spec.tokenizer, TokenizerSpec::Whitespace);
        assert_eq!(spec.case_folding, Folding::Preserve);
        assert_eq!(spec.long_tokens.max_bytes, 32);
    }

    #[test]
    fn default_tokens_fold_case_and_accents() {
        assert_eq!(
            collect_tokens("Beer JALAPEÑO", TokenizerPipelineSpec::tin_default()),
            ["beer", "jalapeno"]
        );
    }

    #[test]
    fn quote_helper_preserves_one_safe_term() {
        assert_eq!(tinql::maybe_quote("beer"), "beer");
        assert_eq!(tinql::maybe_quote("beer cheese"), "\"beer cheese\"");
    }

    #[test]
    fn token_limit_domain_is_checked() {
        let options = TokenizeOptions {
            tokenizer: "unicode",
            case_folding: "fold",
            accent_folding: "fold",
            long_tokens: "split",
            max_token_bytes: 3,
            graphemes: "emoji",
            position_gaps: "preserve",
        };
        assert!(options.into_spec().is_err());
    }
}

/// The index's segment directory: immutable segments and the write buffer.
#[pg_extern(volatile, parallel_unsafe)]
#[allow(clippy::type_complexity)]
fn segment_info(
    index: PgRelation,
) -> TableIterator<
    'static,
    (
        name!(ordinal, i64),
        name!(kind, String),
        name!(root_block, i64),
        name!(docs, i64),
        name!(dead_docs, i64),
        name!(sum_doc_lengths, i64),
        name!(total_pages, i64),
        name!(generation, i64),
    ),
> {
    let tin_name = std::ffi::CString::new("tin").expect("static access method name is valid");
    let tin_am = unsafe { pgrx::pg_sys::get_index_am_oid(tin_name.as_ptr(), false) };
    if unsafe { (*(*index.as_ptr()).rd_rel).relam } != tin_am {
        pgrx::error!("tin.segment_info() requires a tin index");
    }
    if !unsafe { crate::storage::present(index.as_ptr()) } {
        return TableIterator::new(Vec::new());
    }
    let rows = unsafe { crate::storage::segment_rows(index.as_ptr()) };
    TableIterator::new(rows.into_iter().map(|row| {
        (
            row.ordinal,
            row.kind,
            row.root_block,
            row.docs,
            row.dead_docs,
            row.sum_doc_lengths,
            row.total_pages,
            row.generation,
        )
    }))
}
