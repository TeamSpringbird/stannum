// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// Based on Lead, copyright (C) 2026 PlanetScale
//
// See LICENSE in the repository root for license terms.

//! The `==>` operator: the form users write and the index-bound form the
//! planner rewrites it into.
//!
//! `document ==> 'query'` (text, text) knows nothing about the document, so
//! its function tokenizes with the default settings. The function carries a
//! planner support function: when the document operand is a column or
//! expression covered by a stannum index whose predicate the query's
//! restrictions imply, the clause becomes `document ==> indexed_query`, a
//! second operator whose right operand carries the query text and the index
//! OID and whose function tokenizes with that index's settings. Sequential
//! scans, bitmap and custom-scan rechecks, and the operator anywhere else in
//! the statement then agree with the index. Both operators are members of
//! the operator class (strategies 1 and 2), so index paths exist for either.
//!
//! Binding is deterministic: the first covering index by OID (the order of
//! `RelationGetIndexList`) whose predicate holds. A scan of another covering
//! index with different settings is penalized by `amcostestimate` and, if
//! chosen anyway, rechecks every row with the bound settings, so a result
//! never depends on the plan. A clause the support function does not see
//! keeps the default settings: one planned without a query (`expression_planner`
//! has no planner state: index predicates, CHECK constraints, generated
//! columns, trigger WHEN clauses) or one whose document is not a column an
//! index covers (a PL/pgSQL variable, a non-inlined function's argument, a
//! literal). A function cannot recover the relation from its argument at
//! execution time (a `Var` is a range-table position), so nothing resolves
//! these later.
//!
//! An index predicate holding a `==>` clause was therefore evaluated with
//! the defaults, and only a query clause meaning the defaults can prove it.
//! A `get_relation_info` hook rewrites such a predicate clause into the
//! bound form of the query's matching clause when that clause is bound to
//! an index with the default settings, so `predicate_implied_by` sees equal
//! clauses; see [`bind_index_predicates`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::rc::Rc;

#[allow(unused_imports)]
use crate::am::amhandler;
use crate::options::SPEC_BYTES;
#[allow(unused_imports)]
use crate::selectivity::stannum_text_restrict;
use pgrx::{
    FromDatum, Internal, IntoDatum, PgList, PostgresType, extension_sql, pg_extern, pg_guard,
    pg_sys,
};
use serde::{Deserialize, Serialize};
use tinql::runtime::{Query, QueryError, evaluate, parse_tinql_to_query, tokenize_doc};
use tokenizer::{CompiledTokenizerPipeline, TokenizerPipelineSpec};

/// A query bound to the index whose tokenizer settings evaluate it.
#[allow(non_camel_case_types)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, PostgresType)]
#[inoutfuncs]
#[serde(deny_unknown_fields)]
pub struct indexed_query {
    pub index: u32,
    pub query: String,
}

// pgrx's default JSON input returns SQL NULL on decoding errors. PostgreSQL
// type input must reject malformed non-NULL input explicitly.
impl pgrx::InOutFuncs for indexed_query {
    fn input(input: &CStr) -> Self {
        pgrx::inoutfuncs::json_from_slice(input.to_bytes())
            .unwrap_or_else(|error| pgrx::error!("invalid indexed_query: {error}"))
    }

    fn output(&self, buffer: &mut pgrx::StringInfo) {
        let bytes = pgrx::inoutfuncs::json_to_vec(self)
            .unwrap_or_else(|error| pgrx::error!("cannot serialize indexed_query: {error}"));
        buffer.push_bytes(&bytes);
    }
}

// --- Evaluation -----------------------------------------------------------------

/// Parsed queries by tokenizer settings, then by text.
type QueryMemo = HashMap<[u8; SPEC_BYTES], HashMap<String, Rc<Query>>>;

thread_local! {
    /// Parsed queries, so a scan does not re-parse its constant query for
    /// every row.
    static QUERIES: RefCell<QueryMemo> = RefCell::new(HashMap::new());
}

const QUERY_MEMO_LIMIT: usize = 256;

/// Bytes of an invalid query its error message quotes; a longer query (the
/// size limits reject queries of hundreds of kilobytes) is cut, and `...`
/// follows the quotes.
const QUOTED_QUERY_BYTES: usize = 1024;

/// The message of an invalid `==>` query, in TIN's form:
/// `invalid ==> query at byte N in "QUERY": ...`, without the byte when the
/// error names none.
pub(crate) fn invalid_query(text: &str, error: &QueryError) -> String {
    let at = error
        .position()
        .map_or_else(String::new, |byte| format!(" at byte {byte}"));
    let mut end = text.len().min(QUOTED_QUERY_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = if end < text.len() { "..." } else { "" };
    format!(
        "invalid ==> query{at} in {:?}{cut}: {}",
        &text[..end],
        error.detail()
    )
}

/// Parses `text` with `tokenizer`, or raises the error of an invalid query.
pub(crate) fn parse_or_raise<T: tokenizer::Tokenizer>(text: &str, tokenizer: &T) -> Query {
    parse_tinql_to_query(text, tokenizer)
        .unwrap_or_else(|error| pgrx::error!("{}", invalid_query(text, &error)))
}

/// Errors carry their whole message.
fn parsed_query(
    spec: [u8; SPEC_BYTES],
    tokenizer: &CompiledTokenizerPipeline,
    text: &str,
) -> Result<Rc<Query>, String> {
    let memoized = QUERIES.with_borrow(|memo| {
        memo.get(&spec)
            .and_then(|queries| queries.get(text).cloned())
    });
    if let Some(query) = memoized {
        return Ok(query);
    }
    let query = Rc::new(
        parse_tinql_to_query(text, tokenizer).map_err(|error| invalid_query(text, &error))?,
    );
    QUERIES.with_borrow_mut(|memo| {
        let queries = memo.entry(spec).or_default();
        if queries.len() >= QUERY_MEMO_LIMIT {
            queries.clear();
        }
        queries.insert(text.to_owned(), query.clone());
    });
    Ok(query)
}

fn evaluate_with(
    document: &str,
    query: &str,
    spec: [u8; SPEC_BYTES],
    tokenizer: &CompiledTokenizerPipeline,
) -> Result<bool, String> {
    let query = parsed_query(spec, tokenizer, query)?;
    let document = tokenize_doc(document, tokenizer);
    evaluate(&query, &document)
        .map(|result| result.matched)
        .map_err(|error| format!("invalid ==> query: {error}"))
}

fn default_spec() -> [u8; SPEC_BYTES] {
    crate::options::encode_spec(&TokenizerPipelineSpec::stannum_default())
}

fn evaluate_text(document: &str, query_text: &str) -> Result<bool, String> {
    evaluate_with(
        document,
        query_text,
        default_spec(),
        tokenizer::presets::default_pipeline(),
    )
}

/// `text ==> text`: the default tokenizer settings.
#[pg_extern(immutable, parallel_safe)]
pub fn stannum_text_cmpfunc(document: &str, query: &str) -> bool {
    evaluate_text(document, query).unwrap_or_else(|error| pgrx::error!("{error}"))
}

/// `text ==> indexed_query`: the bound index's tokenizer settings.
#[pg_extern(stable, parallel_safe)]
pub fn stannum_text_cmpfunc_indexed(document: &str, query: indexed_query) -> bool {
    let index = unsafe {
        pgrx::PgRelation::with_lock(pg_sys::Oid::from(query.index), pg_sys::AccessShareLock as _)
    };
    crate::udfs::validate_stannum_index(&index, "indexed_query");
    let spec = unsafe { crate::storage::spec_by_oid(pg_sys::Oid::from(query.index)) };
    let tokenizer = crate::storage::tokenizer_for(&spec);
    evaluate_with(document, &query.query, spec, &tokenizer)
        .unwrap_or_else(|error| pgrx::error!("{error}"))
}

/// Binds a non-constant query expression to an index at plan time.
#[pg_extern(immutable, parallel_safe)]
pub fn bind_query(query: &str, index: pg_sys::Oid) -> indexed_query {
    indexed_query {
        index: index.to_u32(),
        query: query.to_owned(),
    }
}

// --- Catalog lookups ------------------------------------------------------------

unsafe fn name_list(names: &[&CStr]) -> *mut pg_sys::List {
    let mut list = PgList::<pg_sys::Node>::new();
    for name in names {
        list.push(unsafe { pg_sys::makeString(pg_sys::pstrdup(name.as_ptr())) }.cast());
    }
    list.into_pg()
}

/// The extension's schema, or `InvalidOid` outside the extension.
///
/// Lookups by qualified name (`LookupTypeNameOid`, `LookupFuncName`) check
/// USAGE on the schema for the current role, and the planner runs them for
/// whoever plans the query, so the catalog is searched directly: a role
/// needs no privilege on the schema to have its clauses bound.
unsafe fn extension_schema() -> pg_sys::Oid {
    unsafe { pg_sys::get_namespace_oid(c"stannum".as_ptr(), true) }
}

/// The OID of `stannum.indexed_query`, or `InvalidOid` outside the extension.
pub(crate) unsafe fn indexed_query_type_oid() -> pg_sys::Oid {
    unsafe {
        let schema = extension_schema();
        if schema == pg_sys::InvalidOid {
            return pg_sys::InvalidOid;
        }
        pg_sys::GetSysCacheOid(
            pg_sys::SysCacheIdentifier::TYPENAMENSP as _,
            pg_sys::Anum_pg_type_oid as _,
            pg_sys::Datum::from(c"indexed_query".as_ptr()),
            pg_sys::Datum::from(schema.to_u32() as usize),
            pg_sys::Datum::from(0_usize),
            pg_sys::Datum::from(0_usize),
        )
    }
}

/// The OID of `stannum.<name>(<types>)`, or `InvalidOid` when absent.
pub(crate) unsafe fn extension_function_oid(name: &CStr, types: &[pg_sys::Oid]) -> pg_sys::Oid {
    unsafe {
        let schema = extension_schema();
        if schema == pg_sys::InvalidOid {
            return pg_sys::InvalidOid;
        }
        let arguments = pg_sys::buildoidvector(types.as_ptr(), types.len() as _);
        pg_sys::GetSysCacheOid(
            pg_sys::SysCacheIdentifier::PROCNAMEARGSNSP as _,
            pg_sys::Anum_pg_proc_oid as _,
            pg_sys::Datum::from(name.as_ptr()),
            pg_sys::Datum::from(arguments),
            pg_sys::Datum::from(schema.to_u32() as usize),
            pg_sys::Datum::from(0_usize),
        )
    }
}

/// `pg_catalog.==>(text, stannum.indexed_query)` and its function.
unsafe fn bound_operator() -> Option<(pg_sys::Oid, pg_sys::Oid)> {
    unsafe {
        let type_oid = indexed_query_type_oid();
        if type_oid == pg_sys::InvalidOid {
            return None;
        }
        let opno = pg_sys::OpernameGetOprid(
            name_list(&[c"pg_catalog", c"==>"]),
            pg_sys::TEXTOID,
            type_oid,
        );
        if opno == pg_sys::InvalidOid {
            return None;
        }
        Some((opno, pg_sys::get_opcode(opno)))
    }
}

unsafe fn bind_query_oid() -> pg_sys::Oid {
    unsafe { extension_function_oid(c"bind_query", &[pg_sys::TEXTOID, pg_sys::OIDOID]) }
}

// --- Constants ------------------------------------------------------------------

/// A text constant, as the planner would make one.
pub(crate) unsafe fn make_text_const(text: &str) -> *mut pg_sys::Node {
    let datum = text.into_datum().expect("a &str is never NULL");
    unsafe {
        pg_sys::makeConst(
            pg_sys::TEXTOID,
            -1,
            pg_sys::DEFAULT_COLLATION_OID,
            -1,
            datum,
            false,
            false,
        )
        .cast()
    }
}

unsafe fn make_oid_const(oid: pg_sys::Oid) -> *mut pg_sys::Node {
    unsafe {
        pg_sys::makeConst(
            pg_sys::OIDOID,
            -1,
            pg_sys::InvalidOid,
            4,
            pg_sys::Datum::from(oid.to_u32() as usize),
            false,
            true,
        )
        .cast()
    }
}

pub(crate) unsafe fn make_indexed_const(query: &str, index: pg_sys::Oid) -> *mut pg_sys::Node {
    let datum = indexed_query {
        index: index.to_u32(),
        query: query.to_owned(),
    }
    .into_datum()
    .expect("a struct is never NULL");
    unsafe {
        pg_sys::makeConst(
            indexed_query_type_oid(),
            -1,
            pg_sys::InvalidOid,
            -1,
            datum,
            false,
            false,
        )
        .cast()
    }
}

// --- Clause recognition ---------------------------------------------------------

/// A `==>` clause in either form.
pub(crate) struct SearchClause {
    pub document: *mut pg_sys::Node,
    /// The query as a text expression; a bound constant yields a fresh
    /// text constant.
    pub query: *mut pg_sys::Node,
    /// The index the clause was bound to at plan time.
    pub index: Option<pg_sys::Oid>,
}

/// Recognizes `document ==> query` with a text or bound right operand.
pub(crate) unsafe fn search_clause(node: *mut pg_sys::Node) -> Option<SearchClause> {
    unsafe {
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_OpExpr {
            return None;
        }
        let op = node.cast::<pg_sys::OpExpr>();
        let name = pg_sys::get_opname((*op).opno);
        if name.is_null()
            || CStr::from_ptr(name).to_bytes() != b"==>"
            || pg_sys::list_length((*op).args) != 2
        {
            return None;
        }
        let document = pg_sys::list_nth((*op).args, 0).cast::<pg_sys::Node>();
        let right = pg_sys::list_nth((*op).args, 1).cast::<pg_sys::Node>();
        if document.is_null() || right.is_null() {
            return None;
        }
        let right_type = pg_sys::exprType(right);
        if right_type == pg_sys::TEXTOID {
            Some(SearchClause {
                document,
                query: right,
                index: None,
            })
        } else if right_type == indexed_query_type_oid() {
            let (query, index) = unbind(right)?;
            Some(SearchClause {
                document,
                query,
                index,
            })
        } else {
            None
        }
    }
}

/// The text query and index of a bound operand: a constant, or
/// `bind_query(text, oid)` around a non-constant query.
unsafe fn unbind(node: *mut pg_sys::Node) -> Option<(*mut pg_sys::Node, Option<pg_sys::Oid>)> {
    unsafe {
        match (*node).type_ {
            pg_sys::NodeTag::T_Const => {
                let value = &*node.cast::<pg_sys::Const>();
                if value.constisnull {
                    return None;
                }
                let bound = indexed_query::from_datum(value.constvalue, false)?;
                Some((
                    make_text_const(&bound.query),
                    Some(pg_sys::Oid::from(bound.index)),
                ))
            }
            pg_sys::NodeTag::T_FuncExpr => {
                let function = &*node.cast::<pg_sys::FuncExpr>();
                if function.funcid != bind_query_oid() || pg_sys::list_length(function.args) != 2 {
                    return None;
                }
                let query = pg_sys::list_nth(function.args, 0).cast::<pg_sys::Node>();
                let index = pg_sys::list_nth(function.args, 1).cast::<pg_sys::Node>();
                let index = ((*index).type_ == pg_sys::NodeTag::T_Const)
                    .then(|| &*index.cast::<pg_sys::Const>())
                    .filter(|value| !value.constisnull && value.consttype == pg_sys::OIDOID)
                    .map(|value| pg_sys::Oid::from(value.constvalue.value() as u32));
                Some((query, index))
            }
            _ => None,
        }
    }
}

/// The text of a constant query operand, in either form.
pub(crate) unsafe fn query_text(node: *mut pg_sys::Node) -> Option<String> {
    unsafe {
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_Const {
            return None;
        }
        let value = &*node.cast::<pg_sys::Const>();
        if value.constisnull {
            return None;
        }
        if value.consttype == pg_sys::TEXTOID {
            String::from_datum(value.constvalue, false)
        } else if value.consttype == indexed_query_type_oid() {
            indexed_query::from_datum(value.constvalue, false).map(|bound| bound.query)
        } else {
            None
        }
    }
}

/// The index a query operand is bound to, when known at plan time.
pub(crate) unsafe fn bound_index(node: *mut pg_sys::Node) -> Option<pg_sys::Oid> {
    unsafe {
        if node.is_null() || pg_sys::exprType(node) != indexed_query_type_oid() {
            return None;
        }
        unbind(node).and_then(|(_, index)| index)
    }
}

struct VarContext {
    varno: i32,
    seen: bool,
    valid: bool,
}

#[pg_guard]
unsafe extern "C-unwind" fn collect_varno(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    if node.is_null() {
        return false;
    }
    let context = unsafe { &mut *context.cast::<VarContext>() };
    if unsafe { (*node).type_ } == pg_sys::NodeTag::T_Var {
        let var = unsafe { &*node.cast::<pg_sys::Var>() };
        if var.varlevelsup != 0 || (context.seen && context.varno != var.varno) {
            context.valid = false;
        } else {
            context.varno = var.varno;
            context.seen = true;
        }
        return false;
    }
    unsafe {
        pg_sys::expression_tree_walker(
            node,
            Some(collect_varno),
            (context as *mut VarContext).cast(),
        )
    }
}

/// The one range-table index an expression's variables refer to.
pub(crate) unsafe fn single_varno(node: *mut pg_sys::Node) -> Option<i32> {
    let mut context = VarContext {
        varno: 0,
        seen: false,
        valid: true,
    };
    unsafe { collect_varno(node, (&mut context as *mut VarContext).cast()) };
    (context.valid && context.seen).then_some(context.varno)
}

// --- Planner support ------------------------------------------------------------

thread_local! {
    /// Set while the support function simplifies restrictions itself, so
    /// the nested pass leaves `==>` clauses alone.
    static REENTRANT: Cell<bool> = const { Cell::new(false) };
}

struct Reentry;

impl Reentry {
    fn enter() -> Self {
        REENTRANT.set(true);
        Self
    }
}

impl Drop for Reentry {
    fn drop(&mut self) {
        REENTRANT.set(false);
    }
}

/// Whether the query's restrictions imply the index's predicate, as the
/// planner will decide once it has built the relation's index list.
unsafe fn predicate_holds(
    root: *mut pg_sys::PlannerInfo,
    varno: i32,
    index_oid: pg_sys::Oid,
) -> bool {
    unsafe {
        let index = pg_sys::index_open(index_oid, pg_sys::AccessShareLock as _);
        let predicate = pg_sys::RelationGetIndexPredicate(index);
        let predicate = if predicate.is_null() {
            std::ptr::null_mut()
        } else {
            pg_sys::copyObjectImpl(predicate.cast()).cast::<pg_sys::List>()
        };
        pg_sys::index_close(index, pg_sys::AccessShareLock as _);
        if predicate.is_null() {
            return true;
        }
        pg_sys::ChangeVarNodes(predicate.cast(), 1, varno, 0);
        let quals = (*(*(*root).parse).jointree).quals;
        if quals.is_null() {
            return false;
        }
        let simplified = {
            let _guard = Reentry::enter();
            pg_sys::eval_const_expressions(root, pg_sys::copyObjectImpl(quals.cast()).cast())
        };
        let clauses = pg_sys::make_ands_implicit(simplified.cast());
        pg_sys::predicate_implied_by(predicate, clauses, false)
    }
}

/// The index a document expression binds to: the first covering stannum
/// index by OID whose predicate holds.
pub(crate) unsafe fn bind_to_index(
    root: *mut pg_sys::PlannerInfo,
    document: *mut pg_sys::Node,
) -> Option<pg_sys::Oid> {
    unsafe {
        let varno = single_varno(document)?;
        let parse = (*root).parse;
        if parse.is_null() || varno < 1 || varno > pg_sys::list_length((*parse).rtable) {
            return None;
        }
        let rte = pg_sys::list_nth((*parse).rtable, varno - 1).cast::<pg_sys::RangeTblEntry>();
        if rte.is_null()
            || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
            || !matches!((*rte).relkind.to_ne_bytes()[0], b'r' | b'p' | b'm')
        {
            return None;
        }
        crate::score::matching_stannum_indexes((*rte).relid, varno, document)
            .into_iter()
            .find(|&index_oid| predicate_holds(root, varno, index_oid))
    }
}

/// The right operand of the bound form: a bound constant for a constant
/// query, `bind_query(query, index)` for anything else.
pub(crate) unsafe fn bound_operand(
    query: *mut pg_sys::Node,
    index: pg_sys::Oid,
) -> Option<*mut pg_sys::Node> {
    unsafe {
        if (*query).type_ == pg_sys::NodeTag::T_Const {
            let value = &*query.cast::<pg_sys::Const>();
            if value.constisnull || value.consttype != pg_sys::TEXTOID {
                return None;
            }
            let text = String::from_datum(value.constvalue, false)?;
            return Some(make_indexed_const(&text, index));
        }
        let function = bind_query_oid();
        if function == pg_sys::InvalidOid {
            return None;
        }
        let mut args = PgList::<pg_sys::Node>::new();
        args.push(pg_sys::copyObjectImpl(query.cast()).cast());
        args.push(make_oid_const(index));
        Some(
            pg_sys::makeFuncExpr(
                function,
                indexed_query_type_oid(),
                args.into_pg(),
                pg_sys::InvalidOid,
                pg_sys::InvalidOid,
                pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
            )
            .cast(),
        )
    }
}

/// Rewrites `document ==> query` to the bound form when a stannum index
/// covers `document`; see the module documentation.
#[pg_extern(immutable, parallel_unsafe)]
fn stannum_text_cmpfunc_support(request: Internal) -> Internal {
    let unhandled = || Internal::from(Some(pg_sys::Datum::from(0_usize)));
    let Some(datum) = request.into_datum() else {
        return unhandled();
    };
    unsafe {
        let node = datum.cast_mut_ptr::<pg_sys::Node>();
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_SupportRequestSimplify {
            return unhandled();
        }
        let request = &*node.cast::<pg_sys::SupportRequestSimplify>();
        if request.root.is_null() || request.fcall.is_null() || REENTRANT.get() {
            return unhandled();
        }
        let args = (*request.fcall).args;
        if pg_sys::list_length(args) != 2 {
            return unhandled();
        }
        let document = pg_sys::list_nth(args, 0).cast::<pg_sys::Node>();
        let query = pg_sys::list_nth(args, 1).cast::<pg_sys::Node>();
        if document.is_null() || query.is_null() {
            return unhandled();
        }
        let Some(index) = bind_to_index(request.root, document) else {
            return unhandled();
        };
        let Some(operand) = bound_operand(query, index) else {
            return unhandled();
        };
        let Some((opno, opfuncid)) = bound_operator() else {
            return unhandled();
        };
        // The plan now holds the index's OID: replan when the index changes.
        let glob = (*request.root).glob;
        if !glob.is_null() {
            (*glob).relationOids = pg_sys::lappend_oid((*glob).relationOids, index);
        }
        let expr = pg_sys::make_opclause(
            opno,
            pg_sys::BOOLOID,
            false,
            pg_sys::copyObjectImpl(document.cast()).cast(),
            operand.cast(),
            pg_sys::InvalidOid,
            (*request.fcall).inputcollid,
        )
        .cast::<pg_sys::OpExpr>();
        (*expr).opfuncid = opfuncid;
        (*expr).location = (*request.fcall).location;
        Internal::from(Some(pg_sys::Datum::from(expr as usize)))
    }
}

// --- Index predicates -----------------------------------------------------------

/// `==>` clauses in the text form (the right operand a text expression).
struct UnboundClauses(Vec<*mut pg_sys::OpExpr>);

#[pg_guard]
unsafe extern "C-unwind" fn collect_unbound(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    if node.is_null() {
        return false;
    }
    unsafe {
        if search_clause(node).is_some() {
            let op = node.cast::<pg_sys::OpExpr>();
            let right = pg_sys::list_nth((*op).args, 1).cast::<pg_sys::Node>();
            if pg_sys::exprType(right) == pg_sys::TEXTOID {
                (*context.cast::<UnboundClauses>()).0.push(op);
            }
        }
        pg_sys::expression_tree_walker(node, Some(collect_unbound), context)
    }
}

/// The `==>` clauses of `node` still in the text form.
unsafe fn unbound_search_clauses(node: *mut pg_sys::Node) -> Vec<*mut pg_sys::OpExpr> {
    let mut clauses = UnboundClauses(Vec::new());
    unsafe { collect_unbound(node, (&mut clauses as *mut UnboundClauses).cast()) };
    clauses.0
}

/// Warns when a stannum index built with other than the default settings
/// has a `==>` clause in its predicate: a predicate is evaluated without a
/// query to bind it against, so with the defaults, whatever the index's
/// settings, and a clause bound to the index cannot prove it.
///
/// # Safety
/// `index` is an open index relation.
pub(crate) unsafe fn warn_about_search_predicate(index: pg_sys::Relation) {
    unsafe {
        let predicate = pg_sys::RelationGetIndexPredicate(index);
        if predicate.is_null() || unbound_search_clauses(predicate.cast()).is_empty() {
            return;
        }
        let spec = crate::options::encode_spec(&crate::options::tokenizer_spec(index));
        if spec == default_spec() {
            return;
        }
        let name = CStr::from_ptr((*(*index).rd_rel).relname.data.as_ptr()).to_string_lossy();
        pgrx::warning!(
            "==> in the predicate of index \"{name}\" uses the default tokenizer settings, \
             not the index's: the index holds the rows the defaults match, and a query \
             bound to the index cannot prove its predicate"
        );
    }
}

static mut PREVIOUS_RELATION_INFO_HOOK: pg_sys::get_relation_info_hook_type = None;

/// Installs the planner hook that lets bound clauses prove `==>` index
/// predicates.
pub fn init() {
    unsafe {
        PREVIOUS_RELATION_INFO_HOOK = pg_sys::get_relation_info_hook;
        pg_sys::get_relation_info_hook = Some(relation_info_hook);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn relation_info_hook(
    root: *mut pg_sys::PlannerInfo,
    relation_oid: pg_sys::Oid,
    inhparent: bool,
    rel: *mut pg_sys::RelOptInfo,
) {
    unsafe {
        if let Some(previous) = PREVIOUS_RELATION_INFO_HOOK {
            previous(root, relation_oid, inhparent, rel);
        }
        bind_index_predicates(root, rel);
    }
}

/// A `==>` clause of the query bound at plan time, with a constant query.
struct BoundClause {
    document: *mut pg_sys::Node,
    query: String,
    index: pg_sys::Oid,
}

struct BoundClauses(Vec<BoundClause>);

#[pg_guard]
unsafe extern "C-unwind" fn collect_bound(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    if node.is_null() {
        return false;
    }
    unsafe {
        if let Some(clause) = search_clause(node)
            && let Some(index) = clause.index
            && let Some(query) = query_text(clause.query)
        {
            (*context.cast::<BoundClauses>()).0.push(BoundClause {
                document: clause.document,
                query,
                index,
            });
        }
        pg_sys::expression_tree_walker(node, Some(collect_bound), context)
    }
}

/// The query's bound clauses (its join tree and the relation's row-security
/// quals), their documents translated to `rel`'s variables when it is a
/// partition or inheritance child, as the planner translates its
/// restrictions.
unsafe fn query_bindings(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
) -> Vec<BoundClause> {
    unsafe {
        let parse = (*root).parse;
        let mut clauses = BoundClauses(Vec::new());
        let context = (&mut clauses as *mut BoundClauses).cast::<c_void>();
        collect_bound((*parse).jointree.cast(), context);
        let top = if (*rel).top_parent.is_null() {
            rel
        } else {
            (*rel).top_parent
        };
        let varno = (*top).relid as i32;
        if varno >= 1 && varno <= pg_sys::list_length((*parse).rtable) {
            let rte = pg_sys::list_nth((*parse).rtable, varno - 1).cast::<pg_sys::RangeTblEntry>();
            collect_bound((*rte).securityQuals.cast(), context);
        }
        if top != rel {
            for clause in &mut clauses.0 {
                clause.document = pg_sys::adjust_appendrel_attrs_multilevel(
                    root,
                    pg_sys::copyObjectImpl(clause.document.cast()).cast(),
                    rel,
                    top,
                );
            }
        }
        clauses.0
    }
}

/// Rewrites each text-form `==>` clause in the predicates of `rel`'s
/// indexes into the bound form of the query's matching clause, when that
/// clause is bound to an index with the default settings.
///
/// The predicate was evaluated with the defaults when the index was built
/// and on every insert (there is no query to bind it against), so a clause
/// meaning the defaults is the same predicate, and rewriting it lets
/// `predicate_implied_by` see equal clauses: the index, of any access
/// method, becomes usable. A clause bound to other settings is a different
/// predicate and never proves it. The rewrite touches the planner's private
/// copy of the predicate only.
unsafe fn bind_index_predicates(root: *mut pg_sys::PlannerInfo, rel: *mut pg_sys::RelOptInfo) {
    unsafe {
        if root.is_null() || rel.is_null() || (*root).parse.is_null() {
            return;
        }
        let mut bindings: Option<Vec<BoundClause>> = None;
        for info in PgList::<pg_sys::IndexOptInfo>::from_pg((*rel).indexlist).iter_ptr() {
            let predicate = (*info).indpred;
            if predicate.is_null() {
                continue;
            }
            for op in unbound_search_clauses(predicate.cast()) {
                let document = pg_sys::list_nth((*op).args, 0).cast::<pg_sys::Node>();
                let Some(text) = query_text(pg_sys::list_nth((*op).args, 1).cast()) else {
                    continue;
                };
                let bindings = bindings.get_or_insert_with(|| query_bindings(root, rel));
                let Some(binding) = bindings.iter().find(|binding| {
                    binding.query == text && pg_sys::equal(binding.document.cast(), document.cast())
                }) else {
                    continue;
                };
                if crate::storage::spec_by_oid(binding.index) != default_spec() {
                    continue;
                }
                let Some((opno, opfuncid)) = bound_operator() else {
                    return;
                };
                let mut args = PgList::<pg_sys::Node>::new();
                args.push(document);
                args.push(make_indexed_const(&text, binding.index));
                (*op).args = args.into_pg();
                (*op).opno = opno;
                (*op).opfuncid = opfuncid;
            }
        }
    }
}

extension_sql!(
    r#"
CREATE OPERATOR pg_catalog.==> (
    PROCEDURE = @extschema@.stannum_text_cmpfunc,
    LEFTARG = pg_catalog.text,
    RIGHTARG = pg_catalog.text,
    RESTRICT = @extschema@.stannum_text_restrict
);

CREATE OPERATOR pg_catalog.==> (
    PROCEDURE = @extschema@.stannum_text_cmpfunc_indexed,
    LEFTARG = pg_catalog.text,
    RIGHTARG = @extschema@.indexed_query,
    RESTRICT = @extschema@.stannum_text_restrict
);

CREATE OPERATOR CLASS @extschema@.stannum_text_ops DEFAULT FOR TYPE pg_catalog.text USING stannum AS
    OPERATOR 1 pg_catalog.==>(pg_catalog.text, pg_catalog.text),
    OPERATOR 2 pg_catalog.==>(pg_catalog.text, @extschema@.indexed_query),
    STORAGE pg_catalog.text;

ALTER FUNCTION @extschema@.stannum_text_cmpfunc(pg_catalog.text, pg_catalog.text)
    SUPPORT @extschema@.stannum_text_cmpfunc_support;
"#,
    name = "stannum_text_operator",
    requires = [
        amhandler,
        indexed_query,
        stannum_text_cmpfunc,
        stannum_text_cmpfunc_indexed,
        stannum_text_cmpfunc_support,
        bind_query,
        stannum_text_restrict
    ]
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

    #[test]
    fn invalid_query_messages_quote_the_query_as_tin_does() {
        let message = |query: &str| {
            let error = tinql::runtime::parse_tinql_to_query_default(query).unwrap_err();
            super::invalid_query(query, &error)
        };
        assert_eq!(
            message("x []"),
            "invalid ==> query at byte 2 in \"x []\": empty alternatives (at byte 2)"
        );
        assert_eq!(
            message("... TO z"),
            "invalid ==> query in \"... TO z\": range bound \"...\" sub-tokenizes into no tokens"
        );
        // A long query is quoted up to a bound.
        let long = message(&("a ".repeat(1000) + "[]"));
        assert!(long.starts_with("invalid ==> query at byte 2000 in \"a a "));
        assert!(long.ends_with(" a \"...: empty alternatives (at byte 2000)"));
        assert!(long.len() < 1200, "{}", long.len());
    }
}
