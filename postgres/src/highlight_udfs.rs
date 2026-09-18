//! `stannum.highlight` and `stannum.highlight_ansi`.
//!
//! Each has two forms. The text-query form analyzes the query and the
//! document with the default tokenizer settings. The planner support
//! function rewrites a call whose document expression is covered by a
//! stannum index (the index a `==>` clause on the same expression is bound
//! to, or the one `==>` itself would bind to) into the `indexed_query`
//! form, which analyzes both with that index's settings, so highlights
//! agree with matches. A NULL query is taken from the `==>` clauses on the
//! same expression, as before.

use crate::highlight::{highlight_text, highlight_text_ansi, positions_from_query, rewrap_text};
use crate::operator::indexed_query;
use pgrx::{Internal, IntoDatum, PgList, default, pg_extern, pg_guard, pg_sys};
use std::borrow::Cow;
use std::ffi::{CStr, c_void};
use tokenizer::CompiledTokenizerPipeline;

fn missing_binding(function: &str) -> ! {
    pgrx::error!("{function} requires an explicit query or a matching stannum index scan")
}

fn render_highlight(
    pipeline: &CompiledTokenizerPipeline,
    text: Option<&str>,
    begin_tag: &str,
    end_tag: &str,
    query: Option<&str>,
) -> Option<String> {
    let text = text?;
    let query = query.unwrap_or_else(|| missing_binding("stannum.highlight()"));
    let positions = positions_from_query(pipeline, query, text);
    highlight_text(pipeline, text, begin_tag, end_tag, &positions)
        .map(Some)
        .unwrap_or_else(|error| pgrx::error!("{error}"))
}

fn render_highlight_ansi(
    pipeline: &CompiledTokenizerPipeline,
    text: Option<&str>,
    wrap_to: Option<i32>,
    query: Option<&str>,
) -> Option<String> {
    let text = text?;
    let query = query.unwrap_or_else(|| missing_binding("stannum.highlight_ansi()"));
    let text = match wrap_to {
        Some(width) if width <= 0 => pgrx::error!("wrap_to must be positive"),
        Some(width) => Cow::Owned(rewrap_text(text, width as usize)),
        None => Cow::Borrowed(text),
    };
    let positions = positions_from_query(pipeline, query, text.as_ref());
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
        query,
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
    render_highlight(&pipeline, text, begin_tag, end_tag, Some(&query.query))
}

#[pg_extern(name = "highlight_ansi", immutable, parallel_safe)]
fn highlight_ansi(
    text: Option<&str>,
    wrap_to: default!(Option<i32>, "NULL"),
    query: default!(Option<&str>, "NULL"),
) -> Option<String> {
    render_highlight_ansi(tokenizer::presets::default_pipeline(), text, wrap_to, query)
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
    render_highlight_ansi(&pipeline, text, wrap_to, Some(&query.query))
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
    if node.is_null() {
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

/// The queries as one text expression: constants are ORed into one
/// constant; anything else keeps the first query.
unsafe fn combined_query(queries: &[*mut pg_sys::Node]) -> *mut pg_sys::Node {
    let first = unsafe { pg_sys::copyObjectImpl(queries[0].cast()).cast() };
    if queries.len() < 2 {
        return first;
    }
    let mut text = Vec::with_capacity(queries.len());
    for &query in queries {
        let Some(value) = (unsafe { crate::operator::query_text(query) }) else {
            return first;
        };
        text.push(format!("({value})"));
    }
    unsafe { crate::operator::make_text_const(&text.join(" OR ")) }
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
        collect_queries(
            (*(*parse).jointree).quals.cast::<pg_sys::Node>(),
            (&mut binding as *mut QueryContext).cast(),
        );
        let implicit = (*supplied_query).type_ == pg_sys::NodeTag::T_Const
            && (*supplied_query.cast::<pg_sys::Const>()).constisnull;
        let query = if implicit {
            if binding.queries.is_empty() {
                return unhandled();
            }
            combined_query(&binding.queries)
        } else {
            supplied_query
        };
        // Analyze as the ==> clause does: with the index it is bound to, or
        // the one it would bind to.
        let index = binding
            .bound
            .or_else(|| crate::operator::bind_to_index(request.root, document));
        let Some(index) = index else {
            return unhandled();
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
            render_highlight(pipeline, Some("Hi there"), "<b>", "</b>", Some("hi")),
            Some("<b>Hi</b> there".into())
        );
        let ansi = render_highlight_ansi(pipeline, Some("hi there"), None, Some("hi")).unwrap();
        assert!(ansi.contains("\x1b["));
        assert!(ansi.contains("hi"));
    }
}
