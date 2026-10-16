-- Gaps between consecutive health samples per (server, pid) on a real tenant.
-- A dead-app alert at threshold T fires falsely whenever a gap > T occurs while the
-- process is alive. Gaps across a pid change (deploy/restart) are excluded.
WITH s AS (
  SELECT server, pid, (julianday(sampled_at) - 2440587.5) * 86400.0 AS t FROM health_samples
), g AS (
  SELECT t - LAG(t) OVER (PARTITION BY server, pid ORDER BY t) AS gap FROM s
)
SELECT COUNT(*) AS gaps,
       ROUND(MIN(gap),1) AS min_s, ROUND(AVG(gap),1) AS avg_s, ROUND(MAX(gap),1) AS max_s,
       SUM(gap > 30) AS over_30s, SUM(gap > 45) AS over_45s, SUM(gap > 60) AS over_60s, SUM(gap > 120) AS over_120s
FROM g WHERE gap IS NOT NULL;
-- Server-level: any process on the host reporting (what a per-host heartbeat sees).
WITH s AS (SELECT server, (julianday(sampled_at) - 2440587.5) * 86400.0 AS t FROM health_samples),
g AS (SELECT t - LAG(t) OVER (PARTITION BY server ORDER BY t) AS gap FROM s)
SELECT 'per-host' AS level, COUNT(*) gaps, ROUND(MAX(gap),1) max_s, SUM(gap > 30) over_30s, SUM(gap > 60) over_60s FROM g WHERE gap IS NOT NULL;
SELECT COUNT(DISTINCT server) servers, COUNT(DISTINCT pid) pids, MIN(sampled_at), MAX(sampled_at) FROM health_samples;
