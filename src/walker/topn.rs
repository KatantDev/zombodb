//! Automatic "top-N by score" pushdown.
//!
//! When a query looks like
//!
//! ```sql
//! SELECT ... WHERE t ==> q ORDER BY zdb.score(t.ctid) DESC LIMIT n [OFFSET m]
//! ```
//!
//! and nothing between the index scan and the LIMIT can *remove* rows, we can
//! ask Elasticsearch for only the top `n + m` hits (sorted by `_score`)
//! instead of scrolling the entire result set.  Postgres still applies its
//! own Sort/Limit/Offset on top, so the results are identical -- there's just
//! dramatically less data to pull from Elasticsearch and fewer heap tuples to
//! fetch.
//!
//! Controlled by the `zdb.score_topn_pushdown` GUC (`off`/`strict`/`primary`).
//! See docs/superpowers/specs/2026-06-11-score-topn-pushdown-design.md

use crate::gucs::{ScoreTopNPushdown, ZDB_LOG_LEVEL, ZDB_SCORE_TOPN_PUSHDOWN};
use crate::utils::lookup_function;
use crate::zdbquery::ZDBQuery;
use pgrx::*;

/// runtime helper the rewriter wraps around the RHS of `==>`.
///
/// Unlike `dsl.limit()` this function is non-STRICT: a `NULL` limit (eg.
/// `LIMIT ALL`, or a NULL bound parameter) leaves the query unchanged, which
/// preserves "no limit" semantics.
#[pg_extern(immutable, parallel_safe)]
fn score_topn_limit(limit: Option<i64>, query: ZDBQuery) -> ZDBQuery {
    match limit {
        Some(limit) if limit >= 0 => query.set_limit(Some(limit as u64)),
        _ => query,
    }
}

struct TopNOids {
    zdbquery: pg_sys::Oid,
    zdb_score: pg_sys::Oid,
    anyelement_cmp: pg_sys::Oid,
    score_topn_limit: pg_sys::Oid,
    dsl_limit: pg_sys::Oid,
    dsl_offset: pg_sys::Oid,
    dsl_offset_limit: pg_sys::Oid,
    int8pl: pg_sys::Oid,
    float8_gt: pg_sys::Oid,
}

impl TopNOids {
    fn lookup() -> Option<TopNOids> {
        let zdbquery = unsafe {
            pg_sys::TypenameGetTypid(
                std::ffi::CStr::from_bytes_with_nul(b"zdbquery\0")
                    .unwrap()
                    .as_ptr(),
            )
        };
        if zdbquery == pg_sys::InvalidOid {
            return None;
        }

        let float8_gt = unsafe {
            let tce = pg_sys::lookup_type_cache(
                pg_sys::FLOAT8OID,
                pg_sys::TYPECACHE_GT_OPR as std::os::raw::c_int,
            );
            (*tce).gt_opr
        };
        if float8_gt == pg_sys::InvalidOid {
            return None;
        }

        Some(TopNOids {
            zdbquery,
            zdb_score: lookup_function(vec!["zdb", "score"], Some(vec![pg_sys::TIDOID]))?,
            anyelement_cmp: lookup_function(
                vec!["zdb", "anyelement_cmpfunc"],
                Some(vec![pg_sys::ANYELEMENTOID, zdbquery]),
            )?,
            score_topn_limit: lookup_function(
                vec!["zdb", "score_topn_limit"],
                Some(vec![pg_sys::INT8OID, zdbquery]),
            )?,
            dsl_limit: lookup_function(
                vec!["dsl", "limit"],
                Some(vec![pg_sys::INT8OID, zdbquery]),
            )?,
            dsl_offset: lookup_function(
                vec!["dsl", "offset"],
                Some(vec![pg_sys::INT8OID, zdbquery]),
            )?,
            dsl_offset_limit: lookup_function(
                vec!["dsl", "offset_limit"],
                Some(vec![pg_sys::INT8OID, pg_sys::INT8OID, zdbquery]),
            )?,
            int8pl: lookup_function(
                vec!["pg_catalog", "int8pl"],
                Some(vec![pg_sys::INT8OID, pg_sys::INT8OID]),
            )?,
            float8_gt,
        })
    }
}

/// lazily-resolved oids: most queries bail out on cheap structural checks
/// before we ever pay for the catalog lookups
type LazyOids = once_cell::unsync::Lazy<Option<TopNOids>>;

/// entry point, called from the planner hook for every top-level Query
pub fn perform(query: &PgBox<pg_sys::Query>) {
    if ZDB_SCORE_TOPN_PUSHDOWN.get() == ScoreTopNPushdown::off {
        return;
    }

    let oids: LazyOids = once_cell::unsync::Lazy::new(TopNOids::lookup);

    unsafe {
        walk_query(query.as_ptr(), &oids);
    }
}

unsafe fn walk_query(query: *mut pg_sys::Query, oids: &LazyOids) {
    if query.is_null() {
        return;
    }

    try_pushdown(query, oids);

    let query = &*query;

    // recurse into subqueries in the range table
    let rtable = PgList::<pg_sys::RangeTblEntry>::from_pg(query.rtable);
    for rte in rtable.iter_ptr() {
        if (*rte).rtekind == pg_sys::RTEKind::RTE_SUBQUERY && !(*rte).subquery.is_null() {
            walk_query((*rte).subquery, oids);
        }
    }

    // ... and into CTEs
    let ctes = PgList::<pg_sys::CommonTableExpr>::from_pg(query.cteList);
    for cte in ctes.iter_ptr() {
        let ctequery = (*cte).ctequery;
        if !ctequery.is_null() && is_a(ctequery, pg_sys::NodeTag::T_Query) {
            walk_query(ctequery as *mut pg_sys::Query, oids);
        }
    }
}

unsafe fn try_pushdown(query: *mut pg_sys::Query, oids: &LazyOids) {
    let q = &mut *query;

    //
    // cheap structural bail-outs: anything that can change the number or
    // identity of rows between the index scan and the LIMIT makes the
    // pushdown unsafe
    //
    if q.commandType != pg_sys::CmdType::CMD_SELECT
        || q.hasAggs
        || q.hasWindowFuncs
        || q.hasTargetSRFs
        || !q.setOperations.is_null()
        || !q.groupClause.is_null()
        || !q.groupingSets.is_null()
        || !q.distinctClause.is_null()
        || !q.windowClause.is_null()
        || !q.havingQual.is_null()
        || !q.rowMarks.is_null()
    {
        return;
    }

    // a plain LIMIT must be present ("WITH TIES" can return more than N rows)
    if q.limitOption != pg_sys::LimitOption::LIMIT_OPTION_COUNT || q.limitCount.is_null() {
        return;
    }

    // `LIMIT ALL` parses to a NULL constant -- that means "no limit"
    if is_a(q.limitCount, pg_sys::NodeTag::T_Const)
        && (*(q.limitCount as *mut pg_sys::Const)).constisnull
    {
        return;
    }

    if q.sortClause.is_null() {
        return;
    }
    let sort_clause = PgList::<pg_sys::SortGroupClause>::from_pg(q.sortClause);
    if ZDB_SCORE_TOPN_PUSHDOWN.get() == ScoreTopNPushdown::strict && sort_clause.len() != 1 {
        return;
    }

    // the cheap structural checks passed -- now resolve the oids we need
    // (they may not exist yet, eg. while CREATE EXTENSION is running)
    let oids = match &**oids {
        Some(oids) => oids,
        None => return,
    };

    // the first sort key must be `zdb.score(rel.ctid) DESC`
    let first_key = match sort_clause.get_ptr(0) {
        Some(sgc) => sgc,
        None => return,
    };
    let scanned_varno = match score_sort_varno(q, first_key, oids) {
        Some(varno) => varno,
        None => return,
    };

    // jointree analysis: find the single `==>` over the scanned relation and
    // prove nothing else can filter its rows
    let opexpr = match analyze_jointree(q, scanned_varno, oids) {
        Some(opexpr) => opexpr,
        None => return,
    };

    let mut args = PgList::<pg_sys::Node>::from_pg((*opexpr).args);
    let rhs = match args.get_ptr(1) {
        Some(rhs) => rhs,
        None => return,
    };

    // if the query already carries an explicit dsl.limit()/dsl.offset(),
    // the application is in control -- don't second-guess it
    if contains_explicit_limit(rhs, oids) {
        return;
    }

    // build `zdb.score_topn_limit(limit [+ offset], rhs)` and swap it in as
    // the new RHS of the `==>` operator
    let size_expr = build_size_expr(q, oids);

    let mut limit_func = PgBox::<pg_sys::FuncExpr>::alloc_node(pg_sys::NodeTag::T_FuncExpr);
    let mut func_args = PgList::<pg_sys::Node>::new();
    func_args.push(size_expr);
    func_args.push(rhs);
    limit_func.funcid = oids.score_topn_limit;
    limit_func.args = func_args.into_pg();
    limit_func.funcresulttype = oids.zdbquery;

    args.pop();
    args.push(limit_func.into_pg() as *mut pg_sys::Node);

    ZDB_LOG_LEVEL
        .get()
        .log("ZDB: score top-N pushdown applied");
}

/// if the first sort key is `zdb.score(rel.ctid) DESC`, returns the varno of
/// `rel`, otherwise None
unsafe fn score_sort_varno(
    q: &pg_sys::Query,
    sgc: *mut pg_sys::SortGroupClause,
    oids: &TopNOids,
) -> Option<i32> {
    // DESC means the sort operator is float8's ">"
    if (*sgc).sortop != oids.float8_gt {
        return None;
    }

    let tlist = PgList::<pg_sys::TargetEntry>::from_pg(q.targetList);
    for te in tlist.iter_ptr() {
        if (*te).ressortgroupref != (*sgc).tleSortGroupRef {
            continue;
        }

        let expr = (*te).expr as *mut pg_sys::Node;
        if !is_a(expr, pg_sys::NodeTag::T_FuncExpr) {
            return None;
        }
        let func = expr as *mut pg_sys::FuncExpr;
        if (*func).funcid != oids.zdb_score {
            return None;
        }

        let func_args = PgList::<pg_sys::Node>::from_pg((*func).args);
        let arg = func_args.get_ptr(0)?;
        if !is_a(arg, pg_sys::NodeTag::T_Var) {
            return None;
        }
        let var = arg as *mut pg_sys::Var;
        if (*var).varlevelsup != 0
            || (*var).varattno != pg_sys::SelfItemPointerAttributeNumber as i16
        {
            return None;
        }
        return Some((*var).varno);
    }

    None
}

/// proves the jointree cannot remove rows of the scanned relation and that
/// its only qual is a single `==>`; returns that OpExpr
unsafe fn analyze_jointree(
    q: &pg_sys::Query,
    scanned_varno: i32,
    oids: &TopNOids,
) -> Option<*mut pg_sys::OpExpr> {
    let jointree = q.jointree;
    if jointree.is_null() {
        return None;
    }

    // the scanned relation must be a plain table
    let rtable = PgList::<pg_sys::RangeTblEntry>::from_pg(q.rtable);
    let rte = rtable.get_ptr((scanned_varno - 1) as usize)?;
    if (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION {
        return None;
    }

    // a single FROM item whose join path to the scanned relation never puts
    // it on a nullable/filterable side
    let fromlist = PgList::<pg_sys::Node>::from_pg((*jointree).fromlist);
    if fromlist.len() != 1 {
        return None;
    }
    if !join_path_is_safe(fromlist.get_ptr(0)?, scanned_varno) {
        return None;
    }

    // the WHERE clause must be exactly one `==>` ...
    let quals = (*jointree).quals;
    if quals.is_null() || !is_a(quals, pg_sys::NodeTag::T_OpExpr) {
        return None;
    }
    let opexpr = quals as *mut pg_sys::OpExpr;
    if (*opexpr).opfuncid != oids.anyelement_cmp {
        return None;
    }

    // ... whose LHS references only the scanned relation
    let args = PgList::<pg_sys::Node>::from_pg((*opexpr).args);
    if args.len() != 2 {
        return None;
    }
    if !references_only(args.get_ptr(0)?, scanned_varno) {
        return None;
    }

    // and it must be the only `==>` in the whole jointree (JOIN ... ON
    // clauses included)
    if count_funcid(jointree as *mut pg_sys::Node, oids.anyelement_cmp, true) != 1 {
        return None;
    }

    Some(opexpr)
}

/// can the join tree remove rows of the scanned relation?  Only LEFT joins
/// with the relation on the outer side (or the bare relation itself) are
/// provably safe
unsafe fn join_path_is_safe(node: *mut pg_sys::Node, scanned_varno: i32) -> bool {
    if is_a(node, pg_sys::NodeTag::T_RangeTblRef) {
        return (*(node as *mut pg_sys::RangeTblRef)).rtindex == scanned_varno;
    }

    if is_a(node, pg_sys::NodeTag::T_JoinExpr) {
        let join = node as *mut pg_sys::JoinExpr;
        let in_left = contains_rtindex((*join).larg, scanned_varno);
        let in_right = contains_rtindex((*join).rarg, scanned_varno);

        return match (*join).jointype {
            pg_sys::JoinType::JOIN_LEFT if in_left => {
                join_path_is_safe((*join).larg, scanned_varno)
            }
            pg_sys::JoinType::JOIN_RIGHT if in_right => {
                join_path_is_safe((*join).rarg, scanned_varno)
            }
            // INNER/FULL/SEMI/ANTI joins (or the relation on the nullable
            // side) can all remove rows
            _ => false,
        };
    }

    false
}

unsafe fn contains_rtindex(node: *mut pg_sys::Node, rtindex: i32) -> bool {
    if node.is_null() {
        return false;
    }
    if is_a(node, pg_sys::NodeTag::T_RangeTblRef) {
        return (*(node as *mut pg_sys::RangeTblRef)).rtindex == rtindex;
    }
    if is_a(node, pg_sys::NodeTag::T_JoinExpr) {
        let join = node as *mut pg_sys::JoinExpr;
        return contains_rtindex((*join).larg, rtindex)
            || contains_rtindex((*join).rarg, rtindex);
    }
    false
}

struct ExprScanContext {
    /// funcids to look for (as a FuncExpr funcid or an OpExpr opfuncid)
    funcids: Vec<pg_sys::Oid>,
    /// when set, also count OpExprs (used to find `==>` operators)
    match_opexprs: bool,
    count: usize,
    /// when looking for Vars: the varno every Var must match
    required_varno: Option<i32>,
    var_count: usize,
    foreign_var_count: usize,
}

#[pg_guard]
unsafe extern "C" fn expr_scan_walker(node: *mut pg_sys::Node, context: void_mut_ptr) -> bool {
    if node.is_null() {
        return false;
    }

    let context = &mut *(context as *mut ExprScanContext);

    if is_a(node, pg_sys::NodeTag::T_FuncExpr) {
        if context
            .funcids
            .contains(&(*(node as *mut pg_sys::FuncExpr)).funcid)
        {
            context.count += 1;
        }
    } else if context.match_opexprs && is_a(node, pg_sys::NodeTag::T_OpExpr) {
        if context
            .funcids
            .contains(&(*(node as *mut pg_sys::OpExpr)).opfuncid)
        {
            context.count += 1;
        }
    } else if is_a(node, pg_sys::NodeTag::T_Var) {
        if let Some(required_varno) = context.required_varno {
            let var = node as *mut pg_sys::Var;
            if (*var).varlevelsup == 0 && (*var).varno == required_varno {
                context.var_count += 1;
            } else {
                context.foreign_var_count += 1;
            }
        }
    }

    pg_sys::expression_tree_walker(node, Some(expr_scan_walker), context as *mut _ as void_mut_ptr)
}

unsafe fn count_funcid(node: *mut pg_sys::Node, funcid: pg_sys::Oid, match_opexprs: bool) -> usize {
    let mut context = ExprScanContext {
        funcids: vec![funcid],
        match_opexprs,
        count: 0,
        required_varno: None,
        var_count: 0,
        foreign_var_count: 0,
    };
    expr_scan_walker(node, &mut context as *mut _ as void_mut_ptr);
    context.count
}

/// does this expression contain an explicit dsl.limit()/dsl.offset()/
/// dsl.offset_limit()/zdb.score_topn_limit() call?
unsafe fn contains_explicit_limit(node: *mut pg_sys::Node, oids: &TopNOids) -> bool {
    let mut context = ExprScanContext {
        funcids: vec![
            oids.dsl_limit,
            oids.dsl_offset,
            oids.dsl_offset_limit,
            oids.score_topn_limit,
        ],
        match_opexprs: false,
        count: 0,
        required_varno: None,
        var_count: 0,
        foreign_var_count: 0,
    };
    expr_scan_walker(node, &mut context as *mut _ as void_mut_ptr);
    context.count > 0
}

/// does this expression reference the scanned relation (and *only* the
/// scanned relation)?
unsafe fn references_only(node: *mut pg_sys::Node, varno: i32) -> bool {
    let mut context = ExprScanContext {
        funcids: vec![],
        match_opexprs: false,
        count: 0,
        required_varno: Some(varno),
        var_count: 0,
        foreign_var_count: 0,
    };
    expr_scan_walker(node, &mut context as *mut _ as void_mut_ptr);
    context.var_count > 0 && context.foreign_var_count == 0
}

/// builds the int8 expression for the ES-side size: `limitCount` or
/// `int8pl(limitOffset, limitCount)` when an OFFSET is present.  OFFSET
/// itself is *not* pushed down -- Postgres still applies it -- we only make
/// sure ES returns enough rows to cover it
unsafe fn build_size_expr(q: &pg_sys::Query, oids: &TopNOids) -> *mut pg_sys::Node {
    let limit = pg_sys::copyObjectImpl(q.limitCount as *const std::os::raw::c_void)
        as *mut pg_sys::Node;

    if q.limitOffset.is_null() {
        return limit;
    }

    let offset = pg_sys::copyObjectImpl(q.limitOffset as *const std::os::raw::c_void)
        as *mut pg_sys::Node;

    let mut add = PgBox::<pg_sys::FuncExpr>::alloc_node(pg_sys::NodeTag::T_FuncExpr);
    let mut add_args = PgList::<pg_sys::Node>::new();
    add_args.push(offset);
    add_args.push(limit);
    add.funcid = oids.int8pl;
    add.args = add_args.into_pg();
    add.funcresulttype = pg_sys::INT8OID;

    add.into_pg() as *mut pg_sys::Node
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::*;

    /// creates `{table}` with 6 rows; for the ZQL query 'beer' the BM25
    /// scores order the ids strictly as 4 > 3 > 2 > 1 (term frequency)
    fn setup(table: &str) {
        Spi::run(&format!("CREATE TABLE {table} (id bigint, body text)")).unwrap();
        Spi::run(&format!(
            "INSERT INTO {table} (id, body) VALUES \
             (1, 'beer'), \
             (2, 'beer beer'), \
             (3, 'beer beer beer'), \
             (4, 'beer beer beer beer'), \
             (5, 'wine'), \
             (6, 'cheese')"
        ))
        .unwrap();
        // shards=1 keeps BM25 statistics global, so the score order above is
        // deterministic
        Spi::run(&format!(
            "CREATE INDEX idx{table} ON {table} USING zombodb (({table}.*)) WITH (shards=1)"
        ))
        .unwrap();
    }

    /// runs EXPLAIN VERBOSE and returns the whole plan text with all
    /// whitespace removed, so we can assert on `"limit":N` regardless of
    /// formatting
    fn explain_squashed(sql: &str) -> String {
        Spi::connect(|client| {
            let mut table =
                client.select(&format!("EXPLAIN (VERBOSE, COSTS OFF) {sql}"), None, &[])?;
            let mut out = String::new();
            while table.next().is_some() {
                out.push_str(&table.get_one::<String>()?.expect("EXPLAIN line was NULL"));
                out.push('\n');
            }
            Ok::<_, spi::Error>(out)
        })
        .expect("EXPLAIN failed")
        .replace([' ', '\n'], "")
    }

    fn ids(sql: &str) -> Vec<i64> {
        Spi::connect(|client| {
            let mut table = client.select(sql, None, &[])?;
            let mut out = Vec::new();
            while table.next().is_some() {
                out.push(table.get_one::<i64>()?.expect("id was NULL"));
            }
            Ok::<_, spi::Error>(out)
        })
        .expect("query failed")
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_pushdown_applies_strict() {
        setup("t_strict");
        let plan = explain_squashed(
            "SELECT id FROM t_strict WHERE t_strict ==> 'beer' \
             ORDER BY zdb.score(ctid) DESC LIMIT 2",
        );
        assert!(
            plan.contains(r#""limit":2"#),
            "expected pushed-down limit in plan: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_pushdown_results_match_off() {
        setup("t_results");
        let query = "SELECT id FROM t_results WHERE t_results ==> 'beer' \
                     ORDER BY zdb.score(ctid) DESC LIMIT 2";

        Spi::run("SET zdb.score_topn_pushdown TO 'off'").unwrap();
        let expected = ids(query);

        Spi::run("SET zdb.score_topn_pushdown TO 'strict'").unwrap();
        let actual = ids(query);

        assert_eq!(expected, vec![4, 3]);
        assert_eq!(actual, expected);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_pushdown_with_offset() {
        setup("t_offset");
        let query = "SELECT id FROM t_offset WHERE t_offset ==> 'beer' \
                     ORDER BY zdb.score(ctid) DESC LIMIT 2 OFFSET 1";

        // limit+offset must be pushed as a single ES-side size...
        let plan = explain_squashed(query);
        assert!(
            plan.contains(r#""limit":3"#),
            "expected limit+offset pushed as 3: {plan}"
        );

        // ...while Postgres still applies the OFFSET itself
        assert_eq!(ids(query), vec![3, 2]);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_no_pushdown_when_off() {
        setup("t_off");
        Spi::run("SET zdb.score_topn_pushdown TO 'off'").unwrap();
        let plan = explain_squashed(
            "SELECT id FROM t_off WHERE t_off ==> 'beer' \
             ORDER BY zdb.score(ctid) DESC LIMIT 2",
        );
        assert!(
            !plan.contains(r#""limit":"#),
            "no limit should be pushed when off: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_no_pushdown_extra_qual() {
        setup("t_qual");
        let plan = explain_squashed(
            "SELECT id FROM t_qual WHERE t_qual ==> 'beer' AND id > 0 \
             ORDER BY zdb.score(ctid) DESC LIMIT 2",
        );
        assert!(
            !plan.contains(r#""limit":"#),
            "extra quals can filter rows, pushdown is unsafe: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_no_pushdown_secondary_sort_strict() {
        setup("t_second");
        let plan = explain_squashed(
            "SELECT id FROM t_second WHERE t_second ==> 'beer' \
             ORDER BY zdb.score(ctid) DESC, id ASC LIMIT 2",
        );
        assert!(
            !plan.contains(r#""limit":"#),
            "strict mode must not push multi-key sorts: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_pushdown_secondary_sort_primary() {
        setup("t_primary");
        Spi::run("SET zdb.score_topn_pushdown TO 'primary'").unwrap();
        let query = "SELECT id FROM t_primary WHERE t_primary ==> 'beer' \
                     ORDER BY zdb.score(ctid) DESC, id ASC LIMIT 2";
        let plan = explain_squashed(query);
        assert!(
            plan.contains(r#""limit":2"#),
            "primary mode allows score as first of several keys: {plan}"
        );
        assert_eq!(ids(query), vec![4, 3]);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_no_pushdown_score_asc() {
        setup("t_asc");
        let plan = explain_squashed(
            "SELECT id FROM t_asc WHERE t_asc ==> 'beer' \
             ORDER BY zdb.score(ctid) ASC LIMIT 2",
        );
        assert!(
            !plan.contains(r#""limit":"#),
            "ascending score sort must not be pushed: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_no_pushdown_with_ties() {
        setup("t_ties");
        let plan = explain_squashed(
            "SELECT id FROM t_ties WHERE t_ties ==> 'beer' \
             ORDER BY zdb.score(ctid) DESC FETCH FIRST 2 ROWS WITH TIES",
        );
        assert!(
            !plan.contains(r#""limit":"#),
            "WITH TIES may return more than N rows, pushdown is unsafe: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_no_pushdown_distinct() {
        setup("t_distinct");
        let plan = explain_squashed(
            "SELECT DISTINCT id, zdb.score(ctid) FROM t_distinct WHERE t_distinct ==> 'beer' \
             ORDER BY zdb.score(ctid) DESC LIMIT 2",
        );
        assert!(
            !plan.contains(r#""limit":"#),
            "DISTINCT changes row counts, pushdown is unsafe: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_explicit_dsl_limit_wins() {
        setup("t_explicit");
        let plan = explain_squashed(
            "SELECT id FROM t_explicit WHERE t_explicit ==> dsl.limit(5, 'beer') \
             ORDER BY zdb.score(ctid) DESC LIMIT 2",
        );
        assert!(
            plan.contains(r#""limit":5"#),
            "explicit dsl.limit must be kept: {plan}"
        );
        assert!(
            !plan.contains(r#""limit":2"#),
            "explicit dsl.limit must not be overridden: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_pushdown_left_join() {
        setup("t_ljoin");
        Spi::run("CREATE TABLE t_ljoin_other (tid bigint, note text)").unwrap();
        Spi::run("INSERT INTO t_ljoin_other (tid, note) VALUES (3, 'three'), (4, 'four')")
            .unwrap();
        let query = "SELECT t_ljoin.id FROM t_ljoin \
                     LEFT JOIN t_ljoin_other ON t_ljoin_other.tid = t_ljoin.id \
                     WHERE t_ljoin ==> 'beer' \
                     ORDER BY zdb.score(t_ljoin.ctid) DESC LIMIT 2";
        let plan = explain_squashed(query);
        assert!(
            plan.contains(r#""limit":2"#),
            "LEFT JOIN cannot remove outer rows, pushdown is safe: {plan}"
        );
        assert_eq!(ids(query), vec![4, 3]);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_no_pushdown_inner_join() {
        setup("t_ijoin");
        Spi::run("CREATE TABLE t_ijoin_other (tid bigint, note text)").unwrap();
        Spi::run("INSERT INTO t_ijoin_other (tid, note) VALUES (1, 'one'), (2, 'two')").unwrap();
        let plan = explain_squashed(
            "SELECT t_ijoin.id FROM t_ijoin \
             JOIN t_ijoin_other ON t_ijoin_other.tid = t_ijoin.id \
             WHERE t_ijoin ==> 'beer' \
             ORDER BY zdb.score(t_ijoin.ctid) DESC LIMIT 2",
        );
        assert!(
            !plan.contains(r#""limit":"#),
            "INNER JOIN can remove rows, pushdown is unsafe: {plan}"
        );
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_pushdown_in_subquery() {
        setup("t_subq");
        let query = "SELECT * FROM (SELECT id FROM t_subq WHERE t_subq ==> 'beer' \
                     ORDER BY zdb.score(ctid) DESC LIMIT 2) sub";
        let plan = explain_squashed(query);
        assert!(
            plan.contains(r#""limit":2"#),
            "pushdown should apply inside subqueries: {plan}"
        );
        assert_eq!(ids(query), vec![4, 3]);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_param_limit_executes() {
        setup("t_param");
        Spi::run(
            "PREPARE topn_p(bigint) AS \
             SELECT id FROM t_param WHERE t_param ==> 'beer' \
             ORDER BY zdb.score(ctid) DESC LIMIT $1",
        )
        .unwrap();
        assert_eq!(ids("EXECUTE topn_p(2)"), vec![4, 3]);
    }

    #[pg_test]
    #[initialize(es = true)]
    fn topn_param_null_limit_means_all() {
        setup("t_pnull");
        Spi::run(
            "PREPARE topn_pn(bigint) AS \
             SELECT id FROM t_pnull WHERE t_pnull ==> 'beer' \
             ORDER BY zdb.score(ctid) DESC LIMIT $1",
        )
        .unwrap();
        // a NULL limit means LIMIT ALL -- pushdown must not break that
        assert_eq!(ids("EXECUTE topn_pn(NULL)"), vec![4, 3, 2, 1]);
    }

    #[pg_test]
    fn topn_guc_defaults_to_strict() -> spi::Result<()> {
        let value = Spi::get_one::<String>("SHOW zdb.score_topn_pushdown")?
            .expect("SHOW returned NULL");
        assert_eq!(value, "strict");
        Ok(())
    }

    #[pg_test]
    fn topn_guc_accepts_all_modes() -> spi::Result<()> {
        for mode in ["off", "strict", "primary"] {
            Spi::run(&format!("SET zdb.score_topn_pushdown TO '{}'", mode))?;
            let value = Spi::get_one::<String>("SHOW zdb.score_topn_pushdown")?
                .expect("SHOW returned NULL");
            assert_eq!(value, mode);
        }
        Ok(())
    }

    #[pg_test(error = "invalid value for parameter \"zdb.score_topn_pushdown\": \"bogus\"")]
    fn topn_guc_rejects_invalid_value() {
        Spi::run("SET zdb.score_topn_pushdown TO 'bogus'").expect("SET should have raised");
    }
}
