#!/usr/bin/env bash
# Dashboard-shaped reads, 5 runs each, warm cache, 1 core. REAL-RS extract (research/IMPROVEMENTS.md).
cd /dev/shm/rwr/cold
G="SELECT group_hash, COUNT(*), AVG(duration), MAX(duration) FROM queries WHERE occurred_at BETWEEN '2026-09-18 17:36:34.315550' AND '2026-09-19 13:04:26.981198' GROUP BY group_hash ORDER BY 2 DESC LIMIT 200"
S="SELECT id, duration, occurred_at, execution_id FROM queries INDEXED BY idx_queries_slowest WHERE occurred_at BETWEEN '2026-09-18 17:36:34.315550' AND '2026-09-19 13:04:26.981198' ORDER BY duration DESC, id DESC LIMIT 51"
SP="SELECT id, duration, occurred_at, execution_id FROM 'queries.zstd.parquet' WHERE occurred_at BETWEEN '2026-09-18 17:36:34.315550' AND '2026-09-19 13:04:26.981198' ORDER BY duration DESC, id DESC LIMIT 51"
GP="SELECT group_hash, COUNT(*), AVG(duration), MAX(duration) FROM 'queries.zstd.parquet' WHERE occurred_at BETWEEN '2026-09-18 17:36:34.315550' AND '2026-09-19 13:04:26.981198' GROUP BY group_hash ORDER BY 2 DESC LIMIT 200"
E="SELECT * FROM queries WHERE execution_id = (SELECT execution_id FROM queries WHERE id = 400000) ORDER BY occurred_at"
EP="SELECT * FROM 'queries.zstd.parquet' WHERE execution_id = (SELECT execution_id FROM 'queries.zstd.parquet' WHERE id = 400000) ORDER BY occurred_at"
t() { local s=$(date +%s%N); "$@" > /dev/null; echo $(( ($(date +%s%N) - s) / 1000000 )); }
for name in G S E; do
  q=${!name}; qp=${name}P; qp=${!qp}
  a=(); b=()
  for i in 1 2 3 4 5; do a+=($(t taskset -c 10 sqlite3 rs.sqlite3 "$q")); b+=($(t taskset -c 10 duckdb -c "SET threads=1; $qp")); done
  echo "$name sqlite ms: ${a[*]}   parquet(duckdb, 1 thread, incl. process start) ms: ${b[*]}"
done
s=$(date +%s%N); for i in 1 2 3 4 5; do duckdb -c "select 1" >/dev/null; done; echo "duckdb process start alone: $(( ($(date +%s%N)-s)/5000000 )) ms"
