-- Copied from railwatch-cloud spike/storage-encoding, with the julianday() time
-- conversion replaced by integer math (the spike found julianday corrupts us).
-- Step 4: a compact physical representation for query events, built from the
-- real extract, measured against the same rows in their current shape.
--
-- Shape of the idea:
--   q_ctx   one row per DISTINCT execution-context snapshot (not per execution:
--           stage/user/tenant/preview can change mid-execution, so the tuple is
--           the snapshot, which is what the child actually captured)
--   q_dict  one row per distinct low-cardinality string, by kind
--   q_shape query_shapes with an integer id
--   q2      the event: integer references, integer time, integer measurements
--
-- Nothing is discarded: every column of every original row is reconstructible
-- by joining q2 -> q_ctx/q_dict/q_shape.
-- Run on a COPY: sqlite3 spike/data/work4.sqlite3 < spike/04_compact_queries.sql
.mode column
.headers on

.print '== baseline: queries table + its indexes, these exact rows'
SELECT (SELECT SUM(pgsize) FROM dbstat WHERE name='queries') AS table_bytes,
       (SELECT SUM(pgsize) FROM dbstat WHERE name IN
          (SELECT name FROM sqlite_schema WHERE type='index' AND tbl_name='queries')) AS index_bytes,
       (SELECT COUNT(*) FROM queries) AS rows;

CREATE TABLE q_ctx (
  id INTEGER PRIMARY KEY,
  execution_id TEXT, trace_id TEXT, execution_preview TEXT,
  execution_source TEXT, execution_stage TEXT,
  server TEXT, deploy TEXT, app_tenant TEXT, user_ref TEXT
);
INSERT INTO q_ctx (execution_id, trace_id, execution_preview, execution_source, execution_stage,
                   server, deploy, app_tenant, user_ref)
SELECT DISTINCT execution_id, trace_id, execution_preview, execution_source, execution_stage,
       server, deploy, app_tenant, user_ref FROM queries;
CREATE UNIQUE INDEX q_ctx_u ON q_ctx (execution_id, trace_id, execution_preview, execution_source,
                                      execution_stage, server, deploy, app_tenant, user_ref);

CREATE TABLE q_dict (id INTEGER PRIMARY KEY, kind INTEGER NOT NULL, value TEXT NOT NULL);
INSERT INTO q_dict (kind, value) SELECT DISTINCT 1, source     FROM queries WHERE source     IS NOT NULL;
INSERT INTO q_dict (kind, value) SELECT DISTINCT 2, name       FROM queries WHERE name       IS NOT NULL;
INSERT INTO q_dict (kind, value) SELECT DISTINCT 3, connection FROM queries WHERE connection IS NOT NULL;
INSERT INTO q_dict (kind, value) SELECT DISTINCT 4, adapter    FROM queries WHERE adapter    IS NOT NULL;
INSERT INTO q_dict (kind, value) SELECT DISTINCT 5, role       FROM queries WHERE role       IS NOT NULL;
CREATE UNIQUE INDEX q_dict_u ON q_dict (kind, value);

CREATE TABLE q_shape (id INTEGER PRIMARY KEY, group_hash TEXT NOT NULL, sql TEXT NOT NULL);
INSERT INTO q_shape (group_hash, sql) SELECT group_hash, sql FROM query_shapes;
CREATE UNIQUE INDEX q_shape_u ON q_shape (group_hash);

-- `at` is microseconds since 2026-01-01 so the integer stays 6 bytes rather than
-- the 7 an absolute epoch needs, against 26 bytes of TEXT today.
CREATE TABLE q2 (
  id INTEGER PRIMARY KEY,
  ctx_id INTEGER NOT NULL,
  shape_id INTEGER,
  at INTEGER NOT NULL,
  duration INTEGER NOT NULL,
  source_id INTEGER, name_id INTEGER, conn_id INTEGER, adapter_id INTEGER, role_id INTEGER,
  allocations INTEGER, row_count INTEGER,
  flags INTEGER NOT NULL DEFAULT 0
);

INSERT INTO q2 (id, ctx_id, shape_id, at, duration, source_id, name_id, conn_id, adapter_id, role_id,
                allocations, row_count, flags)
SELECT q.id,
       c.id,
       s.id,
       (unixepoch(substr(q.occurred_at,1,19)) - unixepoch('2026-01-01')) * 1000000 + CAST(substr(q.occurred_at || '.000000',21,6) AS INTEGER),
       q.duration,
       d1.id, d2.id, d3.id, d4.id, d5.id,
       q.allocations, q.row_count,
       (CASE WHEN q.in_transaction THEN 1 ELSE 0 END) | (CASE WHEN q.async THEN 2 ELSE 0 END)
FROM queries q
JOIN q_ctx c ON c.execution_id IS q.execution_id AND c.trace_id IS q.trace_id
            AND c.execution_preview IS q.execution_preview AND c.execution_source IS q.execution_source
            AND c.execution_stage IS q.execution_stage AND c.server IS q.server
            AND c.deploy IS q.deploy AND c.app_tenant IS q.app_tenant AND c.user_ref IS q.user_ref
LEFT JOIN q_shape s ON s.group_hash = q.group_hash
LEFT JOIN q_dict d1 ON d1.kind = 1 AND d1.value = q.source
LEFT JOIN q_dict d2 ON d2.kind = 2 AND d2.value = q.name
LEFT JOIN q_dict d3 ON d3.kind = 3 AND d3.value = q.connection
LEFT JOIN q_dict d4 ON d4.kind = 4 AND d4.value = q.adapter
LEFT JOIN q_dict d5 ON d5.kind = 5 AND d5.value = q.role;

CREATE INDEX q2_ctx   ON q2 (ctx_id, at);
CREATE INDEX q2_shape ON q2 (shape_id, at);
CREATE INDEX q2_at    ON q2 (at, duration);
CREATE INDEX q2_dur   ON q2 (duration, id);

VACUUM;
ANALYZE;

.print ''
.print '== correctness: every original row must round-trip'
SELECT (SELECT COUNT(*) FROM queries) AS original_rows, (SELECT COUNT(*) FROM q2) AS compact_rows;
SELECT COUNT(*) AS rows_that_do_not_round_trip FROM queries q JOIN q2 ON q2.id = q.id
  JOIN q_ctx c ON c.id = q2.ctx_id
  LEFT JOIN q_shape s ON s.id = q2.shape_id
  LEFT JOIN q_dict d1 ON d1.id = q2.source_id
  LEFT JOIN q_dict d2 ON d2.id = q2.name_id
  LEFT JOIN q_dict d3 ON d3.id = q2.conn_id
WHERE c.execution_id IS NOT q.execution_id OR c.trace_id IS NOT q.trace_id
   OR c.execution_preview IS NOT q.execution_preview OR c.server IS NOT q.server
   OR c.deploy IS NOT q.deploy OR s.group_hash IS NOT q.group_hash
   OR d1.value IS NOT q.source OR d2.value IS NOT q.name OR d3.value IS NOT q.connection
   OR q2.duration IS NOT q.duration OR q2.row_count IS NOT q.row_count;

.print ''
.print '== timestamps must survive the integer round trip to the microsecond'
SELECT COUNT(*) AS timestamps_that_differ FROM queries q JOIN q2 ON q2.id = q.id
WHERE datetime(unixepoch('2026-01-01') + q2.at / 1000000, 'unixepoch') || '.' || printf('%06d', q2.at % 1000000)
   <> substr(q.occurred_at || '.000000', 1, 26);

.print ''
.print '== sizes'
SELECT name, SUM(pgsize) AS bytes FROM dbstat
WHERE name IN ('q2','q2_ctx','q2_shape','q2_at','q2_dur','q_ctx','q_ctx_u','q_dict','q_dict_u','q_shape','q_shape_u')
GROUP BY name ORDER BY bytes DESC;

.print ''
.print '== rows in the side tables (how much context/dictionary we bought)'
SELECT (SELECT COUNT(*) FROM q_ctx) AS ctx_rows, (SELECT COUNT(*) FROM q_dict) AS dict_rows,
       (SELECT COUNT(*) FROM q_shape) AS shape_rows, (SELECT COUNT(*) FROM q2) AS event_rows;

.print ''
.print '== THE COMPARISON: bytes per query event'
SELECT ROUND(1.0 * ((SELECT SUM(pgsize) FROM dbstat WHERE name='queries') +
                    (SELECT SUM(pgsize) FROM dbstat WHERE name IN
                       (SELECT name FROM sqlite_schema WHERE type='index' AND tbl_name='queries')))
             / (SELECT COUNT(*) FROM queries), 1) AS today,
       ROUND(1.0 * (SELECT SUM(pgsize) FROM dbstat WHERE name IN
                      ('q2','q2_ctx','q2_shape','q2_at','q2_dur'))
             / (SELECT COUNT(*) FROM q2), 1) AS compact_events_only,
       ROUND(1.0 * (SELECT SUM(pgsize) FROM dbstat WHERE name IN
                      ('q2','q2_ctx','q2_shape','q2_at','q2_dur',
                       'q_ctx','q_ctx_u','q_dict','q_dict_u','q_shape','q_shape_u'))
             / (SELECT COUNT(*) FROM q2), 1) AS compact_all_in;
