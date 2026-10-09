// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! Binds `document ==> ANY(texts)` and `ALL(texts)` to the index that
//! covers `document`, as `==>` itself is bound (see `operator.rs`).
//!
//! PostgreSQL never asks a support function to simplify a
//! `ScalarArrayOpExpr`, so the operator's support function cannot reach an
//! array search, and its elements would be analyzed with the default
//! tokenizer settings. The planner calls `get_relation_info_hook` after it
//! has preprocessed the query's expressions and before it distributes the
//! quals to relations, so the hook rewrites every array search in place:
//! its operator becomes `==>(text, indexed_query)` and its array an
//! `indexed_query[]` naming the index: a constant for a constant array,
//! otherwise the array coerced element by element through
//! `bind_query(text, index)`. A rewritten search no
//! longer uses the text operator, so a later call leaves it alone. Bitmap
//! index scans take the rewritten search as they take any array search:
//! one scan key per element.
//!
//! The design follows Lead a03e682 ("Support stemming for
//! `body ==> ANY(...)`"), which tags the texts with the analysis instead.

use crate::operator::indexed_query;
use pgrx::{FromDatum, IntoDatum, PgBox, PgList, pg_guard, pg_sys};
use std::ffi::c_void;

/// The `indexed_query[]` datum of `queries`, each bound to `index`; NULL
/// elements stay NULL.
unsafe fn bound_array(
    queries: Vec<Option<String>>,
    index: pg_sys::Oid,
    element_type: pg_sys::Oid,
) -> pg_sys::Datum {
    let mut datums = Vec::with_capacity(queries.len());
    let mut nulls = Vec::with_capacity(queries.len());
    for query in queries {
        let datum = query.and_then(|query| {
            indexed_query {
                index: index.to_u32(),
                query,
            }
            .into_datum()
        });
        nulls.push(datum.is_none());
        datums.push(datum.unwrap_or(pg_sys::Datum::from(0_usize)));
    }
    unsafe {
        let (mut length, mut by_value, mut align) = (0_i16, false, 0 as std::ffi::c_char);
        pg_sys::get_typlenbyvalalign(element_type, &mut length, &mut by_value, &mut align);
        let mut dims = [datums.len() as i32];
        let mut lower_bounds = [1];
        pg_sys::Datum::from(pg_sys::construct_md_array(
            datums.as_mut_ptr(),
            nulls.as_mut_ptr(),
            1,
            dims.as_mut_ptr(),
            lower_bounds.as_mut_ptr(),
            element_type,
            length.into(),
            by_value,
            align,
        ))
    }
}

/// `array` coerced element by element through `bind_query(element, index)`.
unsafe fn bound_array_expression(
    array: *mut pg_sys::Node,
    index: pg_sys::Oid,
    array_type: pg_sys::Oid,
) -> Option<*mut pg_sys::Node> {
    unsafe {
        let function = crate::operator::extension_function_oid(
            c"bind_query",
            &[pg_sys::TEXTOID, pg_sys::OIDOID],
        );
        if function == pg_sys::InvalidOid {
            return None;
        }
        let mut element =
            PgBox::<pg_sys::CaseTestExpr>::alloc_node(pg_sys::NodeTag::T_CaseTestExpr);
        element.typeId = pg_sys::TEXTOID;
        element.typeMod = -1;
        element.collation = pg_sys::DEFAULT_COLLATION_OID;
        let mut args = PgList::<pg_sys::Node>::new();
        args.push(element.into_pg().cast());
        args.push(crate::operator::make_oid_const(index));
        let bind = pg_sys::makeFuncExpr(
            function,
            crate::operator::indexed_query_type_oid(),
            args.into_pg(),
            pg_sys::InvalidOid,
            pg_sys::DEFAULT_COLLATION_OID,
            pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
        );
        let mut coerce =
            PgBox::<pg_sys::ArrayCoerceExpr>::alloc_node(pg_sys::NodeTag::T_ArrayCoerceExpr);
        coerce.arg = array.cast();
        coerce.elemexpr = bind.cast();
        coerce.resulttype = array_type;
        coerce.resulttypmod = -1;
        coerce.resultcollid = pg_sys::InvalidOid;
        coerce.coerceformat = pg_sys::CoercionForm::COERCE_IMPLICIT_CAST;
        coerce.location = -1;
        Some(coerce.into_pg().cast())
    }
}

/// Rewrites the array searches of `root`'s query (not of its subqueries,
/// which the planner plans with their own roots).
///
/// # Safety
/// Called from `get_relation_info_hook` with the planner's root.
pub(crate) unsafe fn bind_array_searches(root: *mut pg_sys::PlannerInfo) {
    unsafe {
        if root.is_null() || (*root).parse.is_null() {
            return;
        }
        let Some(text_operator) = crate::operator::text_operator() else {
            return;
        };
        let mut context = Context {
            root,
            text_operator,
        };
        pg_sys::query_tree_walker_impl((*root).parse, Some(walk), (&raw mut context).cast(), 0);
    }
}

struct Context {
    root: *mut pg_sys::PlannerInfo,
    text_operator: pg_sys::Oid,
}

#[pg_guard]
unsafe extern "C-unwind" fn walk(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    unsafe {
        if node.is_null() || (*node).type_ == pg_sys::NodeTag::T_Query {
            return false;
        }
        if (*node).type_ == pg_sys::NodeTag::T_ScalarArrayOpExpr {
            bind_search(node.cast(), &*context.cast::<Context>());
        }
        pg_sys::expression_tree_walker(node, Some(walk), context)
    }
}

unsafe fn bind_search(search: *mut pg_sys::ScalarArrayOpExpr, context: &Context) {
    unsafe {
        if (*search).opno != context.text_operator || pg_sys::list_length((*search).args) != 2 {
            return;
        }
        let document = pg_sys::list_nth((*search).args, 0).cast::<pg_sys::Node>();
        let array = pg_sys::list_nth((*search).args, 1).cast::<pg_sys::Node>();
        if document.is_null() || array.is_null() || pg_sys::exprType(array) != pg_sys::TEXTARRAYOID
        {
            return;
        }
        let Some(index) = crate::operator::bind_to_index(context.root, document) else {
            return;
        };
        let Some((opno, opfuncid)) = crate::operator::bound_operator() else {
            return;
        };
        let element_type = crate::operator::indexed_query_type_oid();
        let array_type = pg_sys::get_array_type(element_type);
        if array_type == pg_sys::InvalidOid {
            return;
        }
        let bound = if (*array).type_ == pg_sys::NodeTag::T_Const {
            let constant = &*array.cast::<pg_sys::Const>();
            let datum = if constant.constisnull {
                pg_sys::Datum::from(0_usize)
            } else {
                let Some(texts) = Vec::<Option<String>>::from_datum(constant.constvalue, false)
                else {
                    return;
                };
                bound_array(texts, index, element_type)
            };
            pg_sys::makeConst(
                array_type,
                -1,
                pg_sys::InvalidOid,
                -1,
                datum,
                constant.constisnull,
                false,
            )
            .cast::<pg_sys::Node>()
        } else {
            let Some(bound) = bound_array_expression(array, index, array_type) else {
                return;
            };
            bound
        };
        // The plan now holds the index's OID: replan when the index changes.
        let glob = (*context.root).glob;
        if !glob.is_null() {
            (*glob).relationOids = pg_sys::lappend_oid((*glob).relationOids, index);
        }
        (*search).opno = opno;
        (*search).opfuncid = opfuncid;
        (*pg_sys::list_nth_cell((*search).args, 1)).ptr_value = bound.cast();
    }
}
