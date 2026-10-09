// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! Which `==>` clauses of a statement bind to a scored or highlighted
//! relation, and how several of them combine.
//!
//! The scoring and highlighting support functions find a relation's search
//! clauses in the statement's join tree. A clause binds only to the relation
//! its document belongs to, never to a clause under `NOT` (it excludes rows
//! rather than describing them), and the search texts of several clauses are
//! parsed one by one, as `==>` parses each, and ORed: concatenating them could
//! turn texts `==>` rejects into a valid query.

use pgrx::{PgList, pg_guard, pg_sys};
use std::ffi::{CStr, c_void};
use tinql::runtime::{Query, SimplificationProfile, simplify};

/// A `==>` clause on the statement's join tree.
#[derive(Clone, Copy)]
pub(crate) struct Search {
    pub document: *mut pg_sys::Node,
    /// The query as a text expression.
    pub query: *mut pg_sys::Node,
    /// The index the clause was bound to at plan time, if it was.
    pub index: Option<pg_sys::Oid>,
    /// Every row the statement admits satisfies this clause: it is a
    /// top-level conjunct of a qual that restricts its relation.
    pub required: bool,
}

/// Search expressions under a `NOT` exclude rows rather than describe them, so
/// they contribute neither scoring terms nor highlight marks.
pub(crate) unsafe fn is_negation(node: *mut pg_sys::Node) -> bool {
    unsafe {
        (*node).type_ == pg_sys::NodeTag::T_BoolExpr
            && (*node.cast::<pg_sys::BoolExpr>()).boolop == pg_sys::BoolExprType::NOT_EXPR
    }
}

struct Walk {
    searches: Vec<Search>,
    /// Whether the node being visited is a top-level conjunct.
    required: bool,
}

#[pg_guard]
unsafe extern "C-unwind" fn visit_qual(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    unsafe {
        // Sub-selects are other query scopes, and a negated search excludes
        // rows rather than describing them.
        if node.is_null() || (*node).type_ == pg_sys::NodeTag::T_Query || is_negation(node) {
            return false;
        }
        let walk = &mut *context.cast::<Walk>();
        if let Some(clause) = crate::operator::search_clause(node) {
            walk.searches.push(Search {
                document: clause.document,
                query: clause.query,
                index: clause.index,
                required: walk.required,
            });
            return false;
        }
        let conjunction = match (*node).type_ {
            pg_sys::NodeTag::T_List => true,
            pg_sys::NodeTag::T_BoolExpr => {
                (*node.cast::<pg_sys::BoolExpr>()).boolop == pg_sys::BoolExprType::AND_EXPR
            }
            _ => false,
        };
        let required = walk.required;
        walk.required = required && conjunction;
        let stop = pg_sys::expression_tree_walker(node, Some(visit_qual), context);
        walk.required = required;
        stop
    }
}

unsafe fn visit_join_tree(node: *mut pg_sys::Node, nullable: bool, walk: &mut Walk) {
    unsafe {
        if node.is_null() {
            return;
        }
        pg_sys::check_stack_depth();
        let context = (walk as *mut Walk).cast::<c_void>();
        match (*node).type_ {
            pg_sys::NodeTag::T_FromExpr => {
                let from = &*node.cast::<pg_sys::FromExpr>();
                walk.required = true;
                visit_qual(from.quals, context);
                for child in PgList::<pg_sys::Node>::from_pg(from.fromlist).iter_ptr() {
                    visit_join_tree(child, nullable, walk);
                }
            }
            pg_sys::NodeTag::T_JoinExpr => {
                let join = &*node.cast::<pg_sys::JoinExpr>();
                // An inner join's ON clause restricts both sides, whose
                // columns remain visible above the join. Outer, semi and
                // anti joins (pulled-up EXISTS and NOT EXISTS among them)
                // are not scoring predicates: their clauses filter or
                // exclude rows of the other side, or read columns the join
                // does not return.
                let (left, right) = match join.jointype {
                    pg_sys::JoinType::JOIN_LEFT => (nullable, true),
                    pg_sys::JoinType::JOIN_RIGHT => (true, nullable),
                    pg_sys::JoinType::JOIN_FULL => (true, true),
                    _ => (nullable, nullable),
                };
                if join.jointype == pg_sys::JoinType::JOIN_INNER && !nullable {
                    walk.required = true;
                    visit_qual(join.quals, context);
                }
                visit_join_tree(join.larg, left, walk);
                visit_join_tree(join.rarg, right, walk);
            }
            _ => {}
        }
    }
}

/// The search clauses that may bind a scoring call: the WHERE clause, the
/// WHERE clauses of pulled-up subqueries and CTEs (nested `FromExpr`s), and
/// the ON clauses of inner joins outside the nullable side of an outer join,
/// in that order, parents first.
pub(crate) unsafe fn collect_searches(jointree: *mut pg_sys::Node) -> Vec<Search> {
    let mut walk = Walk {
        searches: Vec::new(),
        required: true,
    };
    unsafe { visit_join_tree(jointree, false, &mut walk) };
    walk.searches
}

/// Parses the search texts of the clauses bound to one index into the query
/// scoring or matching evaluates: each text on its own, as `==>` parses it
/// and with the error `==>` raises for it, then ORed and simplified the way
/// lowering simplifies `a OR b`.
pub(crate) fn parse_searches<T: tokenizer::Tokenizer, S: AsRef<str>>(
    texts: &[S],
    tokenizer: &T,
    scoring: bool,
) -> Query {
    let parse = |text: &str| {
        let parsed = if scoring {
            tinql::runtime::parse_tinql_to_scoring_query(text, tokenizer)
        } else {
            tinql::runtime::parse_tinql_to_query(text, tokenizer)
        };
        parsed.unwrap_or_else(|error| {
            crate::operator::raise_query_error(&error, crate::operator::invalid_query(text, &error))
        })
    };
    let mut queries = texts
        .iter()
        .map(|text| parse(text.as_ref()))
        .collect::<Vec<_>>();
    if queries.len() == 1 {
        return queries.remove(0);
    }
    let profile = if scoring {
        SimplificationProfile::StructuralScoring
    } else {
        SimplificationProfile::Structural
    };
    simplify(
        Query::Disjunction {
            min: 1,
            children: queries,
        },
        profile,
    )
}

/// A `text[]` of copies of the text-typed `queries`, each simplified under
/// `root` so a custom plan's bound parameters become constants.
pub(crate) unsafe fn query_array(
    root: *mut pg_sys::PlannerInfo,
    queries: &[*mut pg_sys::Node],
) -> *mut pg_sys::Node {
    unsafe {
        let mut elements = PgList::<pg_sys::Node>::new();
        for &query in queries {
            if query.is_null() || pg_sys::exprType(query) != pg_sys::TEXTOID {
                continue;
            }
            let copy = pg_sys::copyObjectImpl(query.cast()).cast();
            elements.push(pg_sys::eval_const_expressions(root, copy));
        }
        let mut array = pgrx::PgBox::<pg_sys::ArrayExpr>::alloc_node(pg_sys::NodeTag::T_ArrayExpr);
        array.array_typeid = pg_sys::TEXTARRAYOID;
        array.array_collid = pg_sys::DEFAULT_COLLATION_OID;
        array.element_typeid = pg_sys::TEXTOID;
        array.elements = elements.into_pg();
        array.multidims = false;
        array.location = -1;
        // A constant array folds into one `text[]` constant.
        pg_sys::eval_const_expressions(root, array.into_pg().cast())
    }
}

/// The search texts of a bound `text[]` argument; a NULL text matches no
/// rows, so it contributes nothing.
pub(crate) fn search_texts(queries: &[Option<String>]) -> Vec<&str> {
    queries.iter().flatten().map(String::as_str).collect()
}

/// Refuses to score a relation whose searches have no stannum index that may
/// answer them (none covers the document, or each that does is partial with
/// a predicate the query's restrictions do not imply), as TIN does.
pub(crate) unsafe fn refuse_unindexed_scoring(
    rte: *mut pg_sys::RangeTblEntry,
    varno: i32,
    documents: &[*mut pg_sys::Node],
) -> ! {
    let expressions = unsafe {
        let context = pg_sys::deparse_context_for(pg_sys::get_rel_name((*rte).relid), (*rte).relid);
        documents
            .iter()
            .map(|&document| deparse_document(document, varno, context))
            .collect::<Option<Vec<_>>>()
    };
    let detail = match expressions {
        Some(mut expressions) if !expressions.is_empty() => {
            expressions.sort_unstable();
            expressions.dedup();
            format!("No matching stannum index for: {}.", expressions.join(", "))
        }
        _ => "One or more search expressions have no matching stannum index.".to_owned(),
    };
    pg_sys::panic::ErrorReport::new(
        pgrx::PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
        "cannot compute scores for this query",
        pgrx::function_name!(),
    )
    .set_detail(detail)
    .set_hint("Add a matching USING stannum index, or check the definition of an existing index.")
    .report(pgrx::PgLogLevel::ERROR);
    unreachable!("ERROR reports do not return")
}

/// Renders a document expression of relation `varno` against the relation's
/// own name, or `None` for one a single-relation deparse context cannot
/// describe.
unsafe fn deparse_document(
    expression: *mut pg_sys::Node,
    varno: i32,
    context: *mut pg_sys::List,
) -> Option<String> {
    #[pg_guard]
    unsafe extern "C-unwind" fn prepare(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
        if node.is_null() {
            return false;
        }
        unsafe {
            match (*node).type_ {
                pg_sys::NodeTag::T_Var => {
                    let var = &mut *node.cast::<pg_sys::Var>();
                    var.varno = 1;
                    var.varnosyn = 0;
                    var.varattnosyn = 0;
                    var.varnullingrels = std::ptr::null_mut();
                    false
                }
                pg_sys::NodeTag::T_Param => {
                    (*node.cast::<pg_sys::Param>()).paramkind != pg_sys::ParamKind::PARAM_EXTERN
                }
                pg_sys::NodeTag::T_PlaceHolderVar
                | pg_sys::NodeTag::T_SubLink
                | pg_sys::NodeTag::T_SubPlan
                | pg_sys::NodeTag::T_AlternativeSubPlan => true,
                _ => pg_sys::expression_tree_walker(node, Some(prepare), context),
            }
        }
    }
    unsafe {
        if crate::operator::single_varno(expression) != Some(varno) {
            return None;
        }
        let copied = pg_sys::copyObjectImpl(expression.cast()).cast();
        if prepare(copied, std::ptr::null_mut()) {
            return None;
        }
        let rendered = pg_sys::deparse_expression(copied, context, true, false);
        Some(CStr::from_ptr(rendered).to_string_lossy().into_owned())
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn take_nulling(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    unsafe {
        if node.is_null() {
            return false;
        }
        if (*node).type_ == pg_sys::NodeTag::T_Var {
            let ctid = &*context.cast::<pg_sys::Var>();
            let var = &mut *node.cast::<pg_sys::Var>();
            if var.varno == ctid.varno && var.varlevelsup == 0 {
                var.varnullingrels = pg_sys::bms_copy(ctid.varnullingrels);
            }
            return false;
        }
        pg_sys::expression_tree_walker(node, Some(take_nulling), context)
    }
}

/// `CASE WHEN <a search admits the row> THEN score END`: a row that only a
/// qual other than the relation's searches admits, such as the `id = 4` of
/// `body ==> 'gems' OR id = 4`, has no score, as in TIN. `searches` are the
/// relation's scored searches with the index each binds to; the conditions
/// are bound to those indexes, so they match as the scorer reads them.
pub(crate) unsafe fn unless_unsearched(
    root: *mut pg_sys::PlannerInfo,
    ctid: *const pg_sys::Var,
    searches: &[(*mut pg_sys::Node, *mut pg_sys::Node, pg_sys::Oid)],
    score: *mut pg_sys::Node,
) -> Option<*mut pg_sys::Node> {
    unsafe {
        let mut conditions = PgList::<pg_sys::Node>::new();
        for &(document, query, index) in searches {
            let query =
                pg_sys::eval_const_expressions(root, pg_sys::copyObjectImpl(query.cast()).cast());
            let document = pg_sys::copyObjectImpl(document.cast()).cast::<pg_sys::Node>();
            // Above the joins, the document reads the relation's columns as
            // the scored row does.
            take_nulling(document, ctid.cast_mut().cast());
            conditions.push(crate::operator::bound_search(document, query, index)?);
        }
        let condition = if conditions.len() == 1 {
            conditions.get_ptr(0)?
        } else {
            pg_sys::makeBoolExpr(pg_sys::BoolExprType::OR_EXPR, conditions.into_pg(), -1).cast()
        };
        let mut when = pgrx::PgBox::<pg_sys::CaseWhen>::alloc_node(pg_sys::NodeTag::T_CaseWhen);
        when.expr = condition.cast();
        when.result = score.cast();
        when.location = -1;
        let mut whens = PgList::<pg_sys::Node>::new();
        whens.push(when.into_pg().cast());
        let mut case = pgrx::PgBox::<pg_sys::CaseExpr>::alloc_node(pg_sys::NodeTag::T_CaseExpr);
        case.casetype = pg_sys::FLOAT4OID;
        case.casecollid = pg_sys::InvalidOid;
        case.arg = std::ptr::null_mut();
        case.args = whens.into_pg();
        case.defresult = pg_sys::makeConst(
            pg_sys::FLOAT4OID,
            -1,
            pg_sys::InvalidOid,
            4,
            pg_sys::Datum::from(0_usize),
            true,
            true,
        )
        .cast();
        case.location = -1;
        Some(case.into_pg().cast())
    }
}
