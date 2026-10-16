-- Cold tier sizing: the REAL-RS extract (research/IMPROVEMENTS.md) (40k executions, 758,662 queries)
-- as zstd Parquet, sorted by occurred_at (row groups prune on time).
SET threads=2;
CREATE TABLE q AS SELECT * FROM read_csv('queries.csv', header=true, all_varchar=false, timestampformat='%Y-%m-%d %H:%M:%S.%f');
CREATE TABLE e AS SELECT * FROM read_csv('executions.csv', header=true, timestampformat='%Y-%m-%d %H:%M:%S.%f');
COPY (SELECT * FROM q ORDER BY occurred_at) TO 'queries.zstd.parquet' (FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 9, ROW_GROUP_SIZE 122880);
COPY (SELECT * FROM e ORDER BY occurred_at) TO 'executions.zstd.parquet' (FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 9, ROW_GROUP_SIZE 122880);
COPY (SELECT * FROM q ORDER BY occurred_at) TO 'queries.snappy.parquet' (FORMAT parquet, COMPRESSION snappy);
SELECT typeof(occurred_at) t, count(*) FROM q GROUP BY 1;
