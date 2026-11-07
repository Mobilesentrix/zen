# StarRocks: a re-executed prepared statement with a subquery keeps its first parameters

Status: draft upstream bug report (not filed). Workaround in this repo:
`ms/database/src/connection.rs` (`reuses_prepared_statements`): on StarRocks every Lookup query
is prepared, executed and closed; prepared statements are never re-executed.

## What the Rules Engine saw

On admin.dev (StarRocks 4.1.4-4a9848e), a Lookup step whose SELECT had a correlated scalar
subquery and two `{{ }}` parameters returned the row of a run that an earlier request had asked
for, about half the time (asked run 13962, got 14239). The database crate runs every query
through sqlx, which by default prepares a statement once per pooled connection
(`COM_STMT_PREPARE`) and re-executes it (`COM_STMT_EXECUTE`) on later calls. Each pooled
connection kept answering with the parameters of the first execution it had seen.

## Bug report (draft)

**Title:** Re-executing a binary-protocol prepared statement whose query has a subquery returns
the result for the parameters of the first execution

**Versions:** 3.5.20-4d17879 and 4.1.4-4a9848e (`starrocks/allin1-ubuntu`, default settings).
Nothing in the 4.1.4 -> 4.1.7 commit log touches prepared-statement execution except
#79920 (COM_STMT_CLOSE id byte order), so 4.1.7 is expected to behave the same (not run).

**Setup:**

```sql
CREATE DATABASE srfix;
CREATE TABLE srfix.po_plan_run (run_date DATE NOT NULL, run_id BIGINT NOT NULL,
  warehouse_id INT, execution_id VARCHAR(64), algo_name VARCHAR(32), start_time DATETIME)
DUPLICATE KEY(run_date, run_id) PARTITION BY date_trunc('day', run_date)
DISTRIBUTED BY HASH(run_id) BUCKETS 2 PROPERTIES ("replication_num" = "1");
CREATE TABLE srfix.po_plan_run_inputs (run_date DATE NOT NULL, run_id BIGINT NOT NULL,
  settings_cutoff DATETIME)
DUPLICATE KEY(run_date, run_id) PARTITION BY date_trunc('day', run_date)
DISTRIBUTED BY HASH(run_id) BUCKETS 2 PROPERTIES ("replication_num" = "1");
INSERT INTO srfix.po_plan_run VALUES
 ('2026-11-01', 13962, 1, 'e1', 'PO_PLAN', '2026-11-01 01:00:00'),
 ('2026-11-01', 13964, 2, 'e1', 'PO_PLAN', '2026-11-01 01:05:00'),
 ('2026-11-05', 14236, 1, 'e2', 'PO_PLAN', '2026-11-05 01:00:00'),
 ('2026-11-05', 14239, 2, 'e2', 'PO_PLAN', '2026-11-05 01:05:00');
INSERT INTO srfix.po_plan_run_inputs VALUES
 ('2026-11-01', 13962, '2026-10-31 00:00:00'), ('2026-11-01', 13964, '2026-10-31 00:00:00'),
 ('2026-11-05', 14236, '2026-11-04 00:00:00'), ('2026-11-05', 14239, '2026-11-04 00:00:00');
```

**Repro** (binary protocol; Python `mysql-connector-python` 9.1.0, `prepared=True` keeps one
statement id and sends `COM_STMT_EXECUTE` for each call):

```python
import mysql.connector
SQL = ("SELECT r.run_id, (SELECT MAX(i.settings_cutoff) FROM srfix.po_plan_run_inputs i "
       "WHERE i.run_date = r.run_date AND i.run_id = r.run_id) AS c "
       "FROM srfix.po_plan_run r WHERE r.run_date = %s AND r.run_id = %s")
cn = mysql.connector.connect(host="127.0.0.1", port=9030, user="root", password="")
cur = cn.cursor(prepared=True)
for d, rid in [("2026-11-01", 13962), ("2026-11-05", 14239), ("2026-11-01", 13964)]:
    cur.execute(SQL, (d, rid))
    print("asked", rid, "got", cur.fetchall())
```

**Expected:** each call returns the run asked for.
**Actual:** every call after the first returns run 13962 (the first execution's parameters):

```
asked 13962 got [(13962, ...)]
asked 14239 got [(13962, ...)]
asked 13964 got [(13962, ...)]
```

**Matrix** (20 executions per row, same statement id re-executed unless stated; both versions
identical):

| query | wrong |
|---|---|
| no subquery, `WHERE run_date = ? AND run_id = ?` | 0/20 |
| correlated subquery in SELECT, `WHERE run_date = ? AND run_id = ?` | 15/20 (every call after the first) |
| correlated subquery, `WHERE warehouse_id = ? AND run_id = ?` (not the key columns) | wrong as well |
| correlated subquery, one parameter `WHERE run_id = ?` | 0/20 |
| `(SELECT 1)` in SELECT, two parameters | 2nd execution onwards fails: `LogicalProjectOperator cannot be cast to class LogicalOlapScanOperator` |
| same correlated query, new `COM_STMT_PREPARE` for every execution | 0/20 |
| same correlated query, new connection for every execution | 0/20 |
| SQL-level `PREPARE s FROM '...'; SET @d=..., @r=...; EXECUTE s USING @d, @r` (checked on 3.5.20) | correct |

Parameter types do not matter (date as string + id as LONGLONG, or both strings: same result).
The same happens through sqlx 0.8 (Rust) with its default statement cache.

**Where to look (from reading the 4.1.4 FE source, not debugged):**
`ConnectProcessor.handleExecute` -> `StmtExecutor` -> `PrepareStmt.assignValues` sets the new
values on the `Parameter` objects of the statement's single, reused AST
(`fe-parser/.../ast/PrepareStmt.java`), and `PrepareStmtPlanner` caches and re-plans
"point queries" (`QueryStatement.isPointQuery`, all key columns compared with `=`), patching
literals only for a plan rooted at a scan (`rePlanOptimizedPlan` casts the scan's parent chain,
the likely source of the ClassCastException above). The point-query plan cache cannot be the
whole story: the variant filtered on a non-key column is wrong too. Planning a query with a
subquery appears to leave the first execution's values in the reused AST, so later
`assignValues` calls no longer reach them. The SQL-level `EXECUTE` path is not affected.

## Related upstream fix that matters for the workaround

StarRocks #79860 (backports #79920 branch-4.1, #79921 branch-3.5, #79922 branch-4.0): before it,
`COM_STMT_CLOSE` could decode the statement id in the wrong byte order and leave the statement in
the connection's map. On branch-4.1 the shared 16 KiB receive buffer is switched to little-endian
by any small `COM_STMT_EXECUTE`, which hides it; an execute packet over 16 KiB on a connection
that has not yet had a small one leaves the following closes broken. Such statements stay in the
FE until the connection ends (the crate's pools retire connections after `maxLifetime`, default
30 min). In the 4.1 line the fix is in 4.1.7 (not in 4.1.5 / 4.1.6); in 3.5 it is not in 3.5.21.
Recommend 4.1.7 or later for the StarRocks the Rules Engine reads.

## Cost of the workaround

One extra round trip (`COM_STMT_PREPARE`) per query; `COM_STMT_CLOSE` has no reply. Measured on
one warm connection to a local 4.1.4 container (`tests/starrocks_prepared.rs`,
`prepare_cost_per_query`, 300 calls each, debug build):

| query | kept and re-executed (wrong) | prepared every call |
|---|---|---|
| point lookup by (run_date, run_id) | p50 8.57 ms | p50 9.70 ms (+1.1 ms) |
| correlated subquery (above) | p50 14.40 ms | p50 22.51 ms (+8.1 ms) |

The subquery's larger delta is the FE analysing and planning it again; the re-executed figure is
for a plan the FE should not have reused.

## When to remove the workaround

`upstream_starrocks_reexecuted_statement_keeps_first_parameters` in
`ms/database/tests/starrocks_prepared.rs` asserts the bug. When it fails against the StarRocks in
use, the server re-binds parameters correctly; then `reuses_prepared_statements` may return true
for StarRocks again (after running the whole file against that version).
