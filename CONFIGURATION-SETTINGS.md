# Configuration Settings

ZomboDB provides a number of configuration settings that affect how it operates.

## `postgresql.conf`-only settings

The below settings can only be set in `postgresql.conf` and require a Postgres configuration reload (or server restart)
to be changed.

#### `zdb.default_elasticsearch_url`

```
Type: string
Default: null
```

Defines the default URL for your Elasticsearch cluster so you can elite setting it on every index during `CREATE INDEX`.
The value used must end with a forward slash (`/`).

Example: `zdb.default_elasticsearch_url = 'http://es.cluster.ip:9200/'`

#### `zdb.default_replicas`

```
Type: integer
Default: 0
```

Defines the number of replicas all new indices should have. Changing this value does not propagate to existing indices.

## Session-level "GUC" settings

The below settings may be set in `postgresql.conf`, but they can also be changed per session/transaction using Postgres
`SET key TO value` command;

#### `zdb.default_row_estimate`

```
Type: integer
Default: 2500
Range: [-1, INT_MAX]
```

ZomboDB needs to provide Postgres with an estimate of the number of rows Elasticsearch will return for any given query.
2500 is a sensible default estimate that generally convinces Postgres to use an IndexScan plan. Setting this to `-1`
will cause ZomboDB to execute an Elasticsearch `_count` request for every query to return the exact number.

Also note that you can change this parameter at query-time (see [`dsl.row_estimate()`](QUERY-BUILDER-API.md#dslrow_estimate)).

#### `zdb.ignore_visibility`

```
Type: boolean
Default: false
```

ZomboDB applies MVCC visibility rules to all queries and aggregate functions. Setting this to true instructs ZomboDB to
**not** do that, which means aggregate functions (such as `zdb.terms()`) will see dead rows, aborted rows, and in-flight
rows. Generally, this should only be used for debugging.

#### `zdb.log_level`

```
Type: enum
Default: DEBUG1
Possible Values: DEBUG2, DEBUG5, DEBUG4, DEBUG3, DEBUG2, DEBUG1, INFO, NOTICE, WARNING, LOG
```

The Postgres log level ZomboDB sends all of its log messages.

#### `zdb.enable_search_accelerator`

```
Type: boolean
Default: false
```

Indicates if you have the ZomboDB Search Accelerator installed on your backing Elasticsearch cluster.

If you do, ZomboDB is able to highly optimize certain queries, especially those that perform
[cross-index joins](CROSS-INDEX-JOINS.md).

#### `zdb.score_topn_pushdown`

```
Type: enum
Default: strict
Possible Values: off, strict, primary
```

Controls automatic "top-N by score" pushdown.  When a query looks like

```sql
SELECT ... FROM table WHERE table ==> 'query' ORDER BY zdb.score(ctid) DESC LIMIT n [OFFSET m]
```

and ZomboDB can prove that nothing else in the query (additional `WHERE` clauses, inner joins, `DISTINCT`,
aggregation, etc) can remove rows between the index scan and the `LIMIT`, it automatically rewrites the query so
Elasticsearch returns only the top `n + m` hits (sorted by `_score`) instead of scrolling the entire matching result
set.  Postgres still applies its own sort/limit/offset on top, so the results are unchanged -- there's just
dramatically less data pulled from Elasticsearch.

- `strict`:  the pushdown only happens when `zdb.score(...) DESC` is the **only** `ORDER BY` key.  Results are always
  identical to the un-pushed plan.
- `primary`:  the pushdown also happens when `zdb.score(...) DESC` is the **first** of several `ORDER BY` keys
  (eg. `ORDER BY zdb.score(ctid) DESC, popularity DESC`).  Rows with exactly equal scores at the `LIMIT` cutoff may
  then differ from the un-pushed plan, since later sort keys only reorder ties within the top-N that Elasticsearch
  returned.
- `off`:  disables the feature.

Queries that already use an explicit `dsl.limit()`/`dsl.offset()` are never rewritten -- the application stays in
control.
