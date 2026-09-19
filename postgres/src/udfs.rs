// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

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
            collect_tokens("Beer JALAPEÑO", TokenizerPipelineSpec::stannum_default()),
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
    require_stannum_index(&index, "segment_info");
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

pub(crate) fn require_stannum_index(index: &PgRelation, function: &str) {
    validate_stannum_index(index, function);
    require_index_select(index);
}

pub(crate) fn validate_stannum_index(index: &PgRelation, function: &str) {
    let stannum_name =
        std::ffi::CString::new("stannum").expect("static access method name is valid");
    let stannum_am = unsafe { pgrx::pg_sys::get_index_am_oid(stannum_name.as_ptr(), false) };
    if unsafe { (*(*index.as_ptr()).rd_rel).relam } != stannum_am {
        pgrx::error!("stannum.{function}() requires a stannum index");
    }
}

/// Diagnostics expose physical contents, so require table-wide SELECT (column
/// grants and RLS are insufficient), or table ownership. PostgreSQL's ACL
/// check includes pg_read_all_data and inherited role membership.
pub(crate) fn require_index_select(index: &PgRelation) {
    use pgrx::pg_sys;
    unsafe {
        let heap = pg_sys::IndexGetRelation(index.oid(), false);
        let user = pg_sys::GetUserId();
        if pg_sys::object_ownercheck(pg_sys::RelationRelationId, heap, user) {
            return;
        }
        let acl = pg_sys::pg_class_aclcheck(heap, user, pg_sys::ACL_SELECT as _);
        if acl != pg_sys::AclResult::ACLCHECK_OK {
            pg_sys::aclcheck_error(
                acl,
                pg_sys::ObjectType::OBJECT_TABLE,
                pg_sys::get_rel_name(heap),
            );
        }
        // Physical index diagnostics cannot apply row-security policies.
        if pg_sys::check_enable_rls(heap, user, true)
            == pg_sys::CheckEnableRlsResult::RLS_ENABLED as i32
        {
            pgrx::error!("index diagnostics require ownership or SELECT without row security");
        }
    }
}

/// Checks a whole index and lists every inconsistency found; no rows means
/// the index is consistent. `heap_check` also compares the index with the
/// table: every indexed location must exist in the heap and every visible
/// row with indexable tokens must be indexed. See the architecture guide for
/// what each finding means and whether REINDEX or VACUUM resolves it.
#[pg_extern(volatile, parallel_unsafe)]
fn verify_index(
    index: PgRelation,
    heap_check: default!(bool, false),
) -> TableIterator<
    'static,
    (
        name!(severity, String),
        name!(location, String),
        name!(message, String),
    ),
> {
    require_stannum_index(&index, "verify_index");
    let rows = unsafe { crate::storage::verify::verify(index.as_ptr(), heap_check) };
    TableIterator::new(
        rows.into_iter()
            .map(|row| (row.severity, row.location, row.message)),
    )
}

/// Test-only: overwrites raw bytes of an index page in shared buffers, so
/// tests can corrupt an index deliberately and check what the readers say.
#[cfg(feature = "pg_test")]
#[pg_extern(volatile, parallel_unsafe)]
fn corrupt_index_page(index: PgRelation, block: i64, at: i32, bytes: &[u8]) -> i32 {
    if !unsafe { pgrx::pg_sys::superuser() } {
        pgrx::error!("test corruption helpers require superuser");
    }
    require_stannum_index(&index, "corrupt_index_page");
    let block = u32::try_from(block).unwrap_or_else(|_| pgrx::error!("invalid block number"));
    let at = usize::try_from(at).unwrap_or_else(|_| pgrx::error!("invalid page offset"));
    unsafe { crate::storage::verify::corrupt_page(index.as_ptr(), block, at, bytes) };
    bytes.len() as i32
}

/// Test-only: the kind of every page of an index, to pick pages to corrupt.
#[cfg(feature = "pg_test")]
#[pg_extern(volatile, parallel_unsafe)]
fn index_page_kinds(
    index: PgRelation,
) -> TableIterator<'static, (name!(block, i64), name!(kind, String))> {
    require_stannum_index(&index, "index_page_kinds");
    let kinds = unsafe { crate::storage::verify::page_kinds(index.as_ptr()) };
    TableIterator::new(
        kinds
            .into_iter()
            .map(|(block, kind)| (i64::from(block), kind)),
    )
}

/// The installed binary's release version (matches the control-file version).
#[pg_extern(immutable, parallel_safe)]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
