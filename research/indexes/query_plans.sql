-- The gem's read shapes against `queries` (v0.9.1: queries_controller, cursor_page,
-- requests show, filter_query tenant filter, prune), each EXPLAINed after ANALYZE.
-- Indexes that appear in no plan are candidates; the census sizes them.
ANALYZE;
.print '-- index page (slowest, forced)'
EXPLAIN QUERY PLAN SELECT * FROM queries INDEXED BY idx_queries_slowest WHERE occurred_at BETWEEN '2026-10-13 22:20' AND '2026-10-13 22:28' ORDER BY duration DESC, id DESC LIMIT 51;
.print '-- index page with ?q=tenant:acme'
EXPLAIN QUERY PLAN SELECT * FROM queries INDEXED BY idx_queries_slowest WHERE app_tenant='acme' AND occurred_at BETWEEN '2026-10-13 22:20' AND '2026-10-13 22:28' ORDER BY duration DESC, id DESC LIMIT 51;
.print '-- show: group samples'
EXPLAIN QUERY PLAN SELECT * FROM queries WHERE group_hash='x' AND occurred_at BETWEEN '2026-10-13 22:20' AND '2026-10-13 22:28' ORDER BY occurred_at DESC LIMIT 100;
.print '-- show: explain sample'
EXPLAIN QUERY PLAN SELECT * FROM queries WHERE group_hash='x' AND occurred_at BETWEEN '2026-10-13 22:20' AND '2026-10-13 22:28' AND explain IS NOT NULL ORDER BY occurred_at DESC LIMIT 1;
.print '-- request/job show: children'
EXPLAIN QUERY PLAN SELECT * FROM queries WHERE execution_id='x' ORDER BY occurred_at;
.print '-- prune probe'
EXPLAIN QUERY PLAN SELECT id FROM queries WHERE occurred_at < '2026-10-13 22:21' ORDER BY occurred_at LIMIT 5000;
.print '-- grouped window (rollup fallback)'
EXPLAIN QUERY PLAN SELECT group_hash, COUNT(*) FROM queries WHERE occurred_at BETWEEN '2026-10-13 22:20' AND '2026-10-13 22:28' GROUP BY group_hash;
