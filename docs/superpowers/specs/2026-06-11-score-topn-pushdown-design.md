# Score Top-N Pushdown — Design

Date: 2026-06-11
Status: approved (sections 1–2 reviewed interactively)

## Problem

`SELECT ... WHERE idx_func(t) ==> q ORDER BY zdb.score(t.ctid) DESC LIMIT n OFFSET m`
currently makes ZomboDB scroll **all** matching documents from Elasticsearch
(src/elasticsearch/search.rs), fetch a heap tuple for each, and only then lets
Postgres sort and cut `n` rows. For broad fuzzy queries this is thousands of ES
hits and heap fetches per search, while only `n + m` rows are ever returned.

Production consumers (circle_memes, sounds_backend) issue exactly this pattern.

## Solution

During planning (existing planner hook, src/executor_manager/hooks.rs), detect
the "sort by score + LIMIT" pattern and automatically wrap the RHS of `==>` in
`dsl.limit(limit + offset, rhs)`. ES then returns only the top-(n+m) hits,
already sorted by `_score`. Postgres applies its own Sort/Limit on top —
identical results, a fraction of the work.

### Semantics

1. **OFFSET is never pushed down.** Postgres still applies its own OFFSET, so
   we send only `size = limit + offset` to ES. Pushing offset would apply it
   twice.
2. **Safety analysis.** Pushdown only fires when nothing between the index scan
   and LIMIT can *remove* rows. Conditions (all required, per Query level):
   - `CMD_SELECT`; no setops / GROUP BY / DISTINCT / window functions / HAVING /
     grouping sets; `limitOption == LIMIT_OPTION_COUNT` (no `WITH TIES`);
     `limitCount` present (Const or Param both fine).
   - Exactly one `==>` (`anyelement_cmpfunc` OpExpr) at this query level, and it
     is the **only** qual constraining the scanned relation. LEFT JOINs are
     allowed when the scanned relation is on the outer side (they cannot remove
     its rows; duplication is safe — see proof sketch below). Any unrecognized
     construct → conservative bail-out (current behavior preserved).
   - First `ORDER BY` key is `zdb.score(ctid)` of the same relation, direction
     DESC.
   - RHS does not already contain an explicit `dsl.limit` / `dsl.offset` /
     `dsl.offset_limit` call (explicit app control wins; no double-wrapping).
3. **Secondary sort keys.** GUC controls strictness:

   ```
   zdb.score_topn_pushdown = off | strict | primary   (default: strict)
   ```

   - `strict` — pushdown only when `zdb.score DESC` is the *only* sort key
     (exact correctness; ties are arbitrary in vanilla Postgres too).
   - `primary` — score must be the *first* key; later keys (e.g. `usages DESC`)
     may reorder ties. At the size boundary, exact-score ties can differ from
     the unpushed plan. Opt-in for workloads where BM25 ties at the cut line
     are acceptable (e.g. media search).
   - `off` — feature disabled.

   Row-multiplying joins are safe: if every candidate produces ≥1 output row,
   the top-(n+m) score candidates produce ≥ n+m output rows all scoring ≥ any
   excluded candidate, so the final top-(n+m) outputs are among them.

## Architecture

- **GUC** (src/gucs/mod.rs): enum GUC `zdb.score_topn_pushdown`, modeled on
  existing GUCs.
- **Safety analysis** (new module src/walker/topn.rs): per-Query-level analysis
  (unlike the existing tree-global `want_scores` flag), run from
  `PlanWalker::perform`. Resolves `sortClause[0]` → `TargetEntry` → zdb.score
  FuncExpr + DESC sortop (`float4gt`), records the scanned relation's Var, then
  walks the jointree collecting all quals that constrain that relation.
- **Rewrite**: wraps the RHS of the single `==>` in a `FuncExpr` calling
  `dsl.limit(<expr>, rhs)` (oid via existing `lookup_function`). `<expr>` is
  the `limitCount` node as-is, or `int8pl(limitOffset, limitCount)` when OFFSET
  is present. Composes with existing `want_score`/`want_highlight` wrappers.
- **Execution**: no changes needed — `ZDBQuery.limit` already reaches ES as
  `&size=N` and enables `_score` sorting (src/elasticsearch/search.rs).
  Verify only that scrolling stops at `limit` when `limit > 10_000`.
- **Observability**: `debug1`-level log line via `zdb.log_level` machinery when
  pushdown applies ("ZDB: top-N pushdown applied"); used by tests.

## Testing

- `#[pg_test]` tests (Rust, run via `cargo pgrx test pg15`, ES auto-started on
  :19200):
  - correctness: pushdown plan returns identical rows to `off` for
    single-key sort, with LIMIT, LIMIT+OFFSET, bound-parameter LIMIT;
  - fires: simple select, LEFT JOIN outer side, `dsl.min_score` RHS wrapper;
  - does NOT fire: extra WHERE qual, INNER JOIN with qual, GROUP BY, DISTINCT,
    `WITH TIES`, multi-key sort under `strict`, explicit `dsl.limit` in RHS,
    `off` mode, score ASC, score of a different relation.
- Regression safety: full existing `cargo pgrx test` suite must pass with the
  default (`strict`) and with `off`.

## Documentation

- CONFIGURATION-SETTINGS.md: new GUC section.
- THINGS-TO-KNOW.md: short "automatic top-N pushdown" note with safety rules.

## Out of scope (follow-ups for the app repos)

- Replace `SELECT COUNT(*) ... ==> q` with `zdb.count(index, q)` (single
  `_count` request instead of a full scroll).
- Collapse the 3-strategy search ladder into one bool query (up to 6 ES
  round-trips → 1).
- Index `user_id` / `status` in the projection functions so filtered variants
  become ES term filters (making them pushdown-eligible too).
- circle_memes: move the `usages` counter out of the indexed `memes` table
  (every increment currently re-indexes the ES document).
