-- Lossless check: every column of every row, Parquet vs the CSV dump of the SQLite rows.
-- occurred_at compared as the exact SQLite text (strftime with microseconds).
CREATE TABLE src AS SELECT * FROM read_csv('queries.csv', header=true, all_varchar=true);
CREATE TABLE pq AS SELECT * FROM 'queries.zstd.parquet';
SELECT count(*) AS src_rows, (SELECT count(*) FROM pq) AS pq_rows FROM src;
SELECT count(*) AS rows_that_differ FROM src s JOIN pq p ON p.id = CAST(s.id AS BIGINT)
WHERE strftime(p.occurred_at, '%Y-%m-%d %H:%M:%S.%f') IS DISTINCT FROM s.occurred_at
   OR CAST(p.duration AS VARCHAR) IS DISTINCT FROM s.duration
   OR p.execution_id IS DISTINCT FROM s.execution_id OR p.group_hash IS DISTINCT FROM s.group_hash
   OR p.sql IS DISTINCT FROM s.sql OR p.source IS DISTINCT FROM s.source OR p.trace_id IS DISTINCT FROM s.trace_id
   OR p.execution_preview IS DISTINCT FROM s.execution_preview OR CAST(p.row_count AS VARCHAR) IS DISTINCT FROM s.row_count;
SELECT count(*) FILTER (WHERE sql <> '') AS rows_with_inline_sql, count(DISTINCT group_hash) groups FROM src;
