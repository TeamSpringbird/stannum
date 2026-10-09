// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! `stannum.highlight` and `stannum.highlight_ansi`.
//!
//! Each has two forms. The text-query form analyzes the query and the
//! document with the default tokenizer settings. The planner support
//! function rewrites a call whose document expression is covered by a
//! stannum index (the index a `==>` clause on the same expression is bound
//! to, or the one `==>` itself would bind to) into the `indexed_query`
//! form, which analyzes both with that index's settings, so highlights
//! agree with matches. A NULL query is taken from the `==>` clauses on the
//! same expression anywhere in the statement's join tree; with none, the
//! text is returned unmarked, as TIN does.

use crate::highlight::{highlight_text, highlight_text_ansi, positions_from_query, rewrap_text};
use crate::match_positions::MatchPosition;
use crate::operator::indexed_query;
use pgrx::{Internal, IntoDatum, PgList, default, pg_extern, pg_guard, pg_sys};
use std::borrow::Cow;
use std::ffi::{CStr, c_void};
use tokenizer::CompiledTokenizerPipeline;

/// What a highlight marks.
#[derive(Clone, Copy)]
enum Marks<'a> {
    /// Neither an explicit query nor a `==>` clause to take one from: the
    /// text comes back unmarked, as from TIN.
    Nothing,
    /// One query text; one that does not parse marks nothing.
    Query(&'a str),
    /// The search texts of several `==>` clauses, each parsed on its own as
    /// `==>` parses it (raising its error) and ORed.
    Searches(&'a [&'a str]),
}

impl<'a> From<Option<&'a str>> for Marks<'a> {
    fn from(query: Option<&'a str>) -> Self {
        query.map_or(Self::Nothing, Self::Query)
    }
}

impl Marks<'_> {
    fn positions(self, pipeline: &CompiledTokenizerPipeline, text: &str) -> Vec<MatchPosition> {
        match self {
            Self::Nothing => Vec::new(),
            Self::Query(query) => positions_from_query(pipeline, query, text),
            Self::Searches(texts) => {
                let query = crate::score_binding::parse_searches(texts, pipeline, false);
                let document = tinql::runtime::tokenize_doc(text, pipeline);
                tinql::runtime::evaluate_for_highlight(&query, &document)
                    .into_iter()
                    .map(|found| {
                        if found.start == found.end {
                            MatchPosition::point(found.part, found.start)
                        } else {
                            MatchPosition::span(found.part, found.start, found.end)
                        }
                    })
                    .collect()
            }
        }
    }
}

fn render_highlight(
    pipeline: &CompiledTokenizerPipeline,
    text: Option<&str>,
    begin_tag: &str,
    end_tag: &str,
    marks: Marks<'_>,
) -> Option<String> {
    let text = text?;
    if matches!(marks, Marks::Nothing) {
        return Some(text.to_owned());
    }
    let positions = marks.positions(pipeline, text);
    highlight_text(pipeline, text, begin_tag, end_tag, &positions)
        .map(Some)
        .unwrap_or_else(|error| pgrx::error!("{error}"))
}

fn render_highlight_ansi(
    pipeline: &CompiledTokenizerPipeline,
    text: Option<&str>,
    wrap_to: Option<i32>,
    marks: Marks<'_>,
) -> Option<String> {
    let text = text?;
    let text = match wrap_to {
        Some(width) if width <= 0 => pgrx::error!("wrap_to must be positive"),
        Some(width) => Cow::Owned(rewrap_text(text, width as usize)),
        None => Cow::Borrowed(text),
    };
    if matches!(marks, Marks::Nothing) {
        return Some(text.into_owned());
    }
    let positions = marks.positions(pipeline, text.as_ref());
    if positions.is_empty() {
        return Some(text.into_owned());
    }
    highlight_text_ansi(pipeline, text.as_ref(), &positions)
        .map(Some)
        .unwrap_or_else(|error| pgrx::error!("{error}"))
}

#[pg_extern(name = "highlight", immutable, parallel_safe)]
fn highlight(
    text: Option<&str>,
    begin_tag: default!(&str, "'<b>'"),
    end_tag: default!(&str, "'</b>'"),
    query: default!(Option<&str>, "NULL"),
) -> Option<String> {
    render_highlight(
        tokenizer::presets::default_pipeline(),
        text,
        begin_tag,
        end_tag,
        query.into(),
    )
}

#[pg_extern(name = "highlight", stable, parallel_safe)]
fn highlight_bound(
    text: Option<&str>,
    begin_tag: &str,
    end_tag: &str,
    query: indexed_query,
) -> Option<String> {
    let index = unsafe {
        pgrx::PgRelation::with_lock(pg_sys::Oid::from(query.index), pg_sys::AccessShareLock as _)
    };
    crate::udfs::validate_stannum_index(&index, "highlight");
    let pipeline = unsafe { crate::storage::tokenizer_by_oid(pg_sys::Oid::from(query.index)) };
    render_highlight(
        &pipeline,
        text,
        begin_tag,
        end_tag,
        Marks::Query(&query.query),
    )
}

/// `highlight` with the search texts of several `==>` clauses bound to
/// `index`, which the support function rewrites an implicit highlight into.
#[pg_extern(stable, parallel_safe)]
fn highlight_searches(
    text: Option<&str>,
    begin_tag: &str,
    end_tag: &str,
    index: pg_sys::Oid,
    queries: Vec<Option<String>>,
) -> Option<String> {
    let pipeline = searched_pipeline(index);
    let texts = crate::score_binding::search_texts(&queries);
    render_highlight(&pipeline, text, begin_tag, end_tag, Marks::Searches(&texts))
}

/// `highlight_ansi` with the search texts of several `==>` clauses bound to
/// `index`.
#[pg_extern(stable, parallel_safe)]
fn highlight_ansi_searches(
    text: Option<&str>,
    wrap_to: Option<i32>,
    index: pg_sys::Oid,
    queries: Vec<Option<String>>,
) -> Option<String> {
    let pipeline = searched_pipeline(index);
    let texts = crate::score_binding::search_texts(&queries);
    render_highlight_ansi(&pipeline, text, wrap_to, Marks::Searches(&texts))
}

fn searched_pipeline(index: pg_sys::Oid) -> std::rc::Rc<CompiledTokenizerPipeline> {
    let relation = unsafe { pgrx::PgRelation::with_lock(index, pg_sys::AccessShareLock as _) };
    crate::udfs::validate_stannum_index(&relation, "highlight");
    unsafe { crate::storage::tokenizer_by_oid(index) }
}

#[pg_extern(name = "highlight_ansi", immutable, parallel_safe)]
fn highlight_ansi(
    text: Option<&str>,
    wrap_to: default!(Option<i32>, "NULL"),
    query: default!(Option<&str>, "NULL"),
) -> Option<String> {
    render_highlight_ansi(
        tokenizer::presets::default_pipeline(),
        text,
        wrap_to,
        query.into(),
    )
}

#[pg_extern(name = "highlight_ansi", stable, parallel_safe)]
fn highlight_ansi_bound(
    text: Option<&str>,
    wrap_to: Option<i32>,
    query: indexed_query,
) -> Option<String> {
    let index = unsafe {
        pgrx::PgRelation::with_lock(pg_sys::Oid::from(query.index), pg_sys::AccessShareLock as _)
    };
    crate::udfs::validate_stannum_index(&index, "highlight");
    let pipeline = unsafe { crate::storage::tokenizer_by_oid(pg_sys::Oid::from(query.index)) };
    render_highlight_ansi(&pipeline, text, wrap_to, Marks::Query(&query.query))
}

/// The `==>` clauses on one document expression: text query nodes and the
/// index the first bound clause names.
struct QueryContext {
    document: *mut pg_sys::Node,
    queries: Vec<*mut pg_sys::Node>,
    bound: Option<pg_sys::Oid>,
}

#[pg_guard]
unsafe extern "C-unwind" fn collect_queries(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    // A negated search excludes rows rather than describing them, and a
    // sub-select is another query scope.
    if node.is_null()
        || unsafe { (*node).type_ } == pg_sys::NodeTag::T_Query
        || unsafe { crate::score_binding::is_negation(node) }
    {
        return false;
    }
    let context = unsafe { &mut *context.cast::<QueryContext>() };
    if let Some(clause) = unsafe { crate::operator::search_clause(node) }
        && unsafe { pg_sys::equal(clause.document.cast(), context.document.cast()) }
    {
        context.queries.push(clause.query);
        if context.bound.is_none() {
            context.bound = clause.index;
        }
    }
    unsafe {
        pg_sys::expression_tree_walker(
            node,
            Some(collect_queries),
            (context as *mut QueryContext).cast(),
        )
    }
}

fn unhandled() -> Internal {
    Internal::from(Some(pg_sys::Datum::from(0_usize)))
}

/// The overload of `name` taking an `indexed_query` in place of the text
/// query at `query_position`.
unsafe fn bound_overload(name: &CStr, query_position: usize) -> pg_sys::Oid {
    unsafe {
        let mut types = if query_position == 3 {
            vec![pg_sys::TEXTOID, pg_sys::TEXTOID, pg_sys::TEXTOID]
        } else {
            vec![pg_sys::TEXTOID, pg_sys::INT4OID]
        };
        types.push(crate::operator::indexed_query_type_oid());
        crate::operator::extension_function_oid(name, &types)
    }
}

/// The implicit highlight of several `==>` clauses' search texts:
/// `highlight_searches` or `highlight_ansi_searches` with `index` and the
/// texts as an array, which it parses one by one at run time.
unsafe fn searches_call(
    request: &pg_sys::SupportRequestSimplify,
    query_position: usize,
    index: pg_sys::Oid,
    queries: &[*mut pg_sys::Node],
) -> Internal {
    unsafe {
        let (function, types) = if query_position == 3 {
            (
                c"highlight_searches",
                &[
                    pg_sys::TEXTOID,
                    pg_sys::TEXTOID,
                    pg_sys::TEXTOID,
                    pg_sys::OIDOID,
                    pg_sys::TEXTARRAYOID,
                ][..],
            )
        } else {
            (
                c"highlight_ansi_searches",
                &[
                    pg_sys::TEXTOID,
                    pg_sys::INT4OID,
                    pg_sys::OIDOID,
                    pg_sys::TEXTARRAYOID,
                ][..],
            )
        };
        let function = crate::operator::extension_function_oid(function, types);
        if function == pg_sys::InvalidOid {
            return unhandled();
        }
        let mut args = PgList::<pg_sys::Node>::new();
        for position in 0..query_position as i32 {
            args.push(
                pg_sys::copyObjectImpl(pg_sys::list_nth((*request.fcall).args, position).cast())
                    .cast(),
            );
        }
        args.push(
            pg_sys::makeConst(
                pg_sys::OIDOID,
                -1,
                pg_sys::InvalidOid,
                4,
                pg_sys::Datum::from(index.to_u32() as usize),
                false,
                true,
            )
            .cast(),
        );
        args.push(crate::score_binding::query_array(request.root, queries));
        let glob = (*request.root).glob;
        if !glob.is_null() {
            (*glob).relationOids = pg_sys::lappend_oid((*glob).relationOids, index);
        }
        let call = pg_sys::makeFuncExpr(
            function,
            pg_sys::TEXTOID,
            args.into_pg(),
            pg_sys::InvalidOid,
            (*request.fcall).inputcollid,
            pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
        );
        Internal::from(Some(pg_sys::Datum::from(call as usize)))
    }
}

#[pg_extern(immutable, parallel_unsafe)]
fn highlight_support(request: Internal) -> Internal {
    let Some(datum) = request.into_datum() else {
        return unhandled();
    };
    unsafe {
        let node = datum.cast_mut_ptr::<pg_sys::Node>();
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_SupportRequestSimplify {
            return unhandled();
        }
        let request = &*node.cast::<pg_sys::SupportRequestSimplify>();
        if request.root.is_null() || request.fcall.is_null() {
            return unhandled();
        }
        let function_name = pg_sys::get_func_name((*request.fcall).funcid);
        if function_name.is_null() {
            return unhandled();
        }
        let name = CStr::from_ptr(function_name);
        let query_position = match name.to_bytes() {
            b"highlight" => 3,
            b"highlight_ansi" => 2,
            _ => return unhandled(),
        };
        if pg_sys::list_length((*request.fcall).args) <= query_position as i32 {
            return unhandled();
        }
        let supplied_query =
            pg_sys::list_nth((*request.fcall).args, query_position as i32).cast::<pg_sys::Node>();
        if supplied_query.is_null() || pg_sys::exprType(supplied_query) != pg_sys::TEXTOID {
            return unhandled();
        }
        let document = pg_sys::list_nth((*request.fcall).args, 0).cast::<pg_sys::Node>();
        let Some(varno) = crate::operator::single_varno(document) else {
            return unhandled();
        };
        let parse = (*request.root).parse;
        let rte = pg_sys::list_nth((*parse).rtable, varno - 1).cast::<pg_sys::RangeTblEntry>();
        if rte.is_null() || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION {
            return unhandled();
        }
        let mut binding = QueryContext {
            document,
            queries: Vec::new(),
            bound: None,
        };
        // The whole join tree: a subquery or CTE the planner pulled up
        // leaves its WHERE clause in a nested FromExpr, and JOIN ... ON
        // clauses sit in their JoinExpr.
        collect_queries(
            (*parse).jointree.cast::<pg_sys::Node>(),
            (&mut binding as *mut QueryContext).cast(),
        );
        let implicit = (*supplied_query).type_ == pg_sys::NodeTag::T_Const
            && (*supplied_query.cast::<pg_sys::Const>()).constisnull;
        if implicit && binding.queries.is_empty() {
            return unhandled();
        }
        // Analyze as the ==> clause does: with the index it is bound to, or
        // the one it would bind to.
        let index = binding
            .bound
            .or_else(|| crate::operator::bind_to_index(request.root, document));
        let Some(index) = index else {
            return unhandled();
        };
        if implicit && binding.queries.len() > 1 {
            return searches_call(request, query_position, index, &binding.queries);
        }
        let query = if implicit {
            binding.queries[0]
        } else {
            supplied_query
        };
        let Some(operand) = crate::operator::bound_operand(query, index) else {
            return unhandled();
        };
        let overload = bound_overload(name, query_position);
        if overload == pg_sys::InvalidOid {
            return unhandled();
        }
        let replacement = pg_sys::copyObjectImpl(request.fcall.cast()).cast::<pg_sys::FuncExpr>();
        let mut args = PgList::<pg_sys::Node>::new();
        for position in 0..pg_sys::list_length((*request.fcall).args) {
            let argument = if position == query_position as i32 {
                operand
            } else {
                pg_sys::copyObjectImpl(pg_sys::list_nth((*request.fcall).args, position).cast())
                    .cast()
            };
            args.push(argument);
        }
        (*replacement).funcid = overload;
        (*replacement).args = args.into_pg();
        Internal::from(Some(pg_sys::Datum::from(replacement as usize)))
    }
}

pgrx::extension_sql!(
    r#"
ALTER FUNCTION @extschema@.highlight(pg_catalog.text, pg_catalog.text, pg_catalog.text, pg_catalog.text)
    SUPPORT @extschema@.highlight_support;
ALTER FUNCTION @extschema@.highlight_ansi(pg_catalog.text, pg_catalog.int4, pg_catalog.text)
    SUPPORT @extschema@.highlight_support;
"#,
    name = "highlight_support_bindings",
    requires = [
        highlight,
        highlight_ansi,
        highlight_bound,
        highlight_ansi_bound,
        highlight_searches,
        highlight_ansi_searches,
        highlight_support
    ]
);

#[cfg(feature = "pg_test")]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use pgrx::pg_test;

    #[pg_test]
    fn explicit_html_and_ansi_highlighting_render_matches() {
        let pipeline = tokenizer::presets::default_pipeline();
        assert_eq!(
            render_highlight(pipeline, Some("Hi there"), "<b>", "</b>", Some("hi").into()),
            Some("<b>Hi</b> there".into())
        );
        let ansi =
            render_highlight_ansi(pipeline, Some("hi there"), None, Some("hi").into()).unwrap();
        assert!(ansi.contains("\x1b["));
        assert!(ansi.contains("hi"));
    }
}
