-- Sample data for the Solar Energy Dashboard.
--
-- Creates the `solarpower` table (the app's default TABLE_NAME) matching the
-- real inverter export schema, and fills it with a realistic solar curve for
-- the last 3 days at 10-minute intervals.
--
-- Key points so this matches what the app expects:
--   * `wattsperhour` is a CUMULATIVE daily watt-hour counter: it resets to 0
--     each day and climbs to that day's total (daily kWh = MAX/1000).
--   * `invertertime` is an ISO-8601 string with a TZ offset (parsed by the app
--     when PARSE_ISO_TIME=true, the default).
--   * `watts` is instantaneous power.
--
-- Run it with:
--   mysql -u <user> -p <database> < seed.sql
--
-- Requires MySQL 8.0+ or MariaDB 10.2+ (recursive CTEs + window functions).

CREATE TABLE IF NOT EXISTS solarpower (
    watts         INT             NOT NULL,
    wattsperhour  INT UNSIGNED    NOT NULL,
    invertertime  VARCHAR(45)     NOT NULL,
    created_at    DATETIME        DEFAULT CURRENT_TIMESTAMP,
    id            INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    PRIMARY KEY (id)
);

-- Re-seeding? Uncomment to clear existing rows first.
-- TRUNCATE TABLE solarpower;

INSERT INTO solarpower (watts, wattsperhour, invertertime, created_at)
WITH RECURSIVE
-- 3 days * 24 hours * 6 (10-min slots) = 432 steps
seq AS (
    SELECT 0 AS n
    UNION ALL
    SELECT n + 1 FROM seq WHERE n + 1 < (3 * 24 * 6)
),
points AS (
    SELECT
        DATE_ADD(DATE_SUB(CURDATE(), INTERVAL 2 DAY), INTERVAL n * 10 MINUTE) AS ts
    FROM seq
),
calc AS (
    SELECT ts, HOUR(ts) + MINUTE(ts) / 60.0 AS hod FROM points
),
w AS (
    SELECT
        ts,
        -- Daylight ~06:00-20:00; sine bell peaking midday, ~4.5 kW peak.
        CASE
            WHEN hod >= 6 AND hod <= 20 THEN
                GREATEST(0, ROUND(
                    4500 * POW(SIN(PI() * (hod - 6) / 14), 1.4) * (0.9 + 0.1 * RAND()),
                0))
            ELSE 0
        END AS watts
    FROM calc
)
SELECT
    CAST(watts AS SIGNED) AS watts,
    -- Cumulative Wh within each day: running sum of watts * (10 min / 60).
    CAST(
        COALESCE(SUM(watts * 10.0 / 60.0) OVER (PARTITION BY DATE(ts) ORDER BY ts), 0)
    AS UNSIGNED) AS wattsperhour,
    -- ISO-8601 with a TZ offset, like the real inverter data.
    CONCAT(DATE_FORMAT(ts, '%Y-%m-%dT%H:%i:%s'), '-04:00') AS invertertime,
    ts AS created_at
FROM w
ORDER BY ts;
