-- sqsh test environment: generate ~1 million rows in analytics.events
-- Uses a doubling technique (INSERT ... SELECT FROM self) to avoid
-- writing millions of literal INSERT rows in this file.

SET NAMES utf8mb4;
USE analytics;

-- Disable autocommit and indexes for faster bulk insert
SET autocommit = 0;
SET unique_checks = 0;
SET foreign_key_checks = 0;

-- ---- Step 1: Seed 1024 distinct base rows -------------------------
-- 4^5 = 1024 rows via 5-way cross join on a 4-row inline table.
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT
    CASE (a.n + b.n + c.n + d.n + e.n) MOD 6
        WHEN 0 THEN 'page_view'
        WHEN 1 THEN 'click'
        WHEN 2 THEN 'scroll'
        WHEN 3 THEN 'form_submit'
        WHEN 4 THEN 'purchase'
        ELSE        'search'
    END AS event_type,
    1 + (a.n * 251 + b.n * 103 + c.n * 67 + d.n * 41 + e.n * 17) MOD 10000 AS user_id,
    CASE (a.n + b.n * 2 + c.n) MOD 10
        WHEN 0 THEN '/home'
        WHEN 1 THEN '/products'
        WHEN 2 THEN '/product/detail'
        WHEN 3 THEN '/cart'
        WHEN 4 THEN '/checkout'
        WHEN 5 THEN '/account'
        WHEN 6 THEN '/search'
        WHEN 7 THEN '/blog'
        WHEN 8 THEN '/about'
        ELSE        '/contact'
    END AS page_url,
    CASE (a.n + d.n) MOD 5
        WHEN 0 THEN 'https://google.com'
        WHEN 1 THEN 'https://twitter.com'
        WHEN 2 THEN NULL
        WHEN 3 THEN 'https://bing.com'
        ELSE        'https://facebook.com'
    END AS referrer,
    CONCAT(
        10 + a.n, '.', 20 + b.n, '.', 30 + c.n, '.', 40 + d.n
    ) AS ip_address,
    CASE (a.n + b.n + e.n) MOD 5
        WHEN 0 THEN 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/120.0'
        WHEN 1 THEN 'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) Safari/605.1'
        WHEN 2 THEN 'Mozilla/5.0 (X11; Linux x86_64) Firefox/121.0'
        WHEN 3 THEN 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0) Mobile/15E148'
        ELSE        'Mozilla/5.0 (Android 14; Mobile) Chrome/120.0'
    END AS user_agent,
    DATE_ADD('2023-01-01 00:00:00',
        INTERVAL (a.n * 86400 + b.n * 3600 + c.n * 600 + d.n * 60 + e.n * 10) SECOND
    ) AS created_at
FROM
    (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) a,
    (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) b,
    (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) c,
    (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) d,
    (SELECT 0 AS n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3) e;

COMMIT;

-- ---- Step 2: Double the table repeatedly -------------------------
-- Each INSERT...SELECT doubles the row count.
-- After 10 doublings: 1024 * 2^10 = 1,048,576 rows (~1M).

-- Round 1: 1024 -> 2048
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 100 DAY)
FROM events;
COMMIT;

-- Round 2: 2048 -> 4096
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 200 DAY)
FROM events;
COMMIT;

-- Round 3: 4096 -> 8192
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 400 DAY)
FROM events;
COMMIT;

-- Round 4: 8192 -> 16384
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 800 DAY)
FROM events;
COMMIT;

-- Round 5: 16384 -> 32768
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 1600 DAY)
FROM events;
COMMIT;

-- Round 6: 32768 -> 65536
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 3200 DAY)
FROM events;
COMMIT;

-- Round 7: 65536 -> 131072
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 6400 DAY)
FROM events;
COMMIT;

-- Round 8: 131072 -> 262144
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 12800 DAY)
FROM events;
COMMIT;

-- Round 9: 262144 -> 524288
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 25600 DAY)
FROM events;
COMMIT;

-- Round 10: 524288 -> 1048576
INSERT INTO events (event_type, user_id, page_url, referrer, ip_address, user_agent, created_at)
SELECT event_type, user_id, page_url, referrer, ip_address, user_agent,
    DATE_ADD(created_at, INTERVAL 51200 DAY)
FROM events;
COMMIT;

-- Restore session settings
SET autocommit = 1;
SET unique_checks = 1;
SET foreign_key_checks = 1;
