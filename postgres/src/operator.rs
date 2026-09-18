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
//! (no planner state, as in index predicates and generated columns; a
//! document expression no index covers) keeps the default settings.

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
use tinql::runtime::{Query, evaluate, parse_tinql_to_query, tokenize_doc};
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
    let query = Rc::new(parse_tinql_to_query(text, tokenizer).map_err(|e| e.to_string())?);
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
        .map_err(|e| e.to_string())
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
    evaluate_text(document, query)
        .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"))
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
        .unwrap_or_else(|error| pgrx::error!("invalid ==> query: {error}"))
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

/// The OID of `stannum.indexed_query`, or `InvalidOid` outside the extension.
pub(crate) unsafe fn indexed_query_type_oid() -> pg_sys::Oid {
    unsafe {
        let name = pg_sys::makeTypeNameFromNameList(name_list(&[c"stannum", c"indexed_query"]));
        pg_sys::LookupTypeNameOid(std::ptr::null_mut(), name, true)
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
    unsafe {
        let types = [pg_sys::TEXTOID, pg_sys::OIDOID];
        pg_sys::LookupFuncName(
            name_list(&[c"stannum", c"bind_query"]),
            types.len() as i32,
            types.as_ptr(),
            true,
        )
    }
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
}
