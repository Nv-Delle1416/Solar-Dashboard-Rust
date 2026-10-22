-- Mock data for the Watts Dashboard.
--
-- Creates the `readings` table (matching the app's default TABLE_NAME) and
-- fills it with a realistic solar-production curve: zero overnight, ramping up
-- after sunrise, peaking around midday, and tapering off before sunset.
--
-- Covers the last 3 days at 15-minute intervals (~288 rows).
--
-- Run it with either:
--   mysql -u <user> -p <database> < seed.sql
--   -- or paste into any MySQL client connected to your database.
--
-- Requires MySQL 8.0+ or MariaDB 10.2+ (recursive CTEs + window functions).

CREATE TABLE IF NOT EXISTS readings (
    id            INT AUTO_INCREMENT PRIMARY KEY,
    Watts         DOUBLE       NOT NULL,
    wattsperhour  DOUBLE       NOT NULL,
    invertertime  DATETIME     NOT NULL,
    created_at    TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Re-seeding? Uncomment the next line to clear existing rows first.
-- TRUNCATE TABLE readings;

INSERT INTO readings (Watts, wattsperhour, invertertime, created_at)
WITH RECURSIVE
-- 3 days * 24 hours * 4 (15-min slots) = 288 steps
seq AS (
    SELECT 0 AS n
    UNION ALL
    SELECT n + 1 FROM seq WHERE n + 1 < (3 * 24 * 4)
),
points AS (
    SELECT
        DATE_ADD(
            DATE_SUB(CURDATE(), INTERVAL 2 DAY),
            INTERVAL n * 15 MINUTE
        ) AS ts
    FROM seq
),
calc AS (
    SELECT
        ts,
        HOUR(ts) + MINUTE(ts) / 60.0 AS hod   -- hour-of-day as a decimal
    FROM points
),
watts AS (
    SELECT
        ts,
        -- Daylight window ~06:00 to ~20:00; sine bell curve peaking midday,
        -- with a little random "cloud" variation. Peak ~5.2 kW.
        CASE
            WHEN hod >= 6 AND hod <= 20 THEN
                GREATEST(0, ROUND(
                    5200
                    * POW(SIN(PI() * (hod - 6) / 14), 1.6)
                    * (0.88 + 0.12 * RAND()),
                1))
            ELSE 0
        END AS watts
    FROM calc
)
SELECT
    watts,
    -- Cumulative energy (Wh) generated so far each day: watts * 0.25h per slot.
    ROUND(SUM(watts * 0.25) OVER (PARTITION BY DATE(ts) ORDER BY ts), 1) AS wattsperhour,
    ts AS invertertime,
    ts AS created_at
FROM watts
ORDER BY ts;
