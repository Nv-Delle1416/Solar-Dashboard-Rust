use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};
use sqlx::mysql::MySqlPoolOptions;
use sqlx::MySqlPool;

/// How to divide daily energy into an hourly average.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AverageBasis {
    /// Divide by a fixed 24 hours (whole-day average, includes night).
    FixedDay,
    /// Divide by the daylight span (first-to-last reading each day).
    Daylight,
}

impl AverageBasis {
    fn parse(s: &str) -> AverageBasis {
        match s.trim().to_ascii_lowercase().as_str() {
            "daylight" => AverageBasis::Daylight,
            _ => AverageBasis::FixedDay,
        }
    }
}

/// Runtime configuration, loaded from environment variables (see `.env.example`).
#[derive(Clone)]
struct Config {
    database_url: String,
    table_name: String,
    time_column: String,
    energy_column: String,
    parse_iso_time: bool,
    average_basis: AverageBasis,
    bind_addr: SocketAddr,
    default_limit: i64,
    interval_minutes: f64,
    mock_data: bool,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let mock_data = std::env::var("MOCK_DATA")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);

        // In mock mode we never touch a database, so DATABASE_URL is optional.
        let database_url = if mock_data {
            std::env::var("DATABASE_URL").unwrap_or_default()
        } else {
            std::env::var("DATABASE_URL")
                .context("DATABASE_URL must be set (e.g. mysql://user:pass@host:3306/dbname)")?
        };

        let table_name = std::env::var("TABLE_NAME").unwrap_or_else(|_| "solarpower".to_string());
        validate_identifier(&table_name).context("invalid TABLE_NAME")?;

        let time_column =
            std::env::var("TIME_COLUMN").unwrap_or_else(|_| "invertertime".to_string());
        validate_identifier(&time_column).context("invalid TIME_COLUMN")?;

        let energy_column =
            std::env::var("ENERGY_COLUMN").unwrap_or_else(|_| "wattsperhour".to_string());
        validate_identifier(&energy_column).context("invalid ENERGY_COLUMN")?;

        // `invertertime` is an ISO-8601 string with a TZ offset; parse it to a
        // real datetime for grouping/windowing. Set false if the time column is
        // already a DATETIME/TIMESTAMP.
        let parse_iso_time = std::env::var("PARSE_ISO_TIME")
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);

        let average_basis = std::env::var("AVERAGE_BASIS")
            .map(|v| AverageBasis::parse(&v))
            .unwrap_or(AverageBasis::FixedDay);

        let bind_addr: SocketAddr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .context("BIND_ADDR must be a valid socket address, e.g. 127.0.0.1:8080")?;

        let default_limit = std::env::var("DEFAULT_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);

        // Sampling interval between rows, used to convert power (W) into energy
        // (kWh): energy = Watts / 1000 * (interval_minutes / 60).
        let interval_minutes = std::env::var("INTERVAL_MINUTES")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(15.0);

        Ok(Self {
            database_url,
            table_name,
            time_column,
            energy_column,
            parse_iso_time,
            average_basis,
            bind_addr,
            default_limit,
            interval_minutes,
            mock_data,
        })
    }
}

/// Guard against SQL injection: table/column names cannot be bound as parameters,
/// so we only allow simple identifiers (letters, digits, underscore).
fn validate_identifier(s: &str) -> anyhow::Result<()> {
    if s.is_empty()
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(anyhow!(
            "'{s}' is not a valid SQL identifier (allowed: letters, digits, underscore)"
        ));
    }
    Ok(())
}

struct AppState {
    pool: Option<MySqlPool>,
    config: Config,
}

/// A single point on the chart: a label (time / day / month) and a kWh value.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct ChartPoint {
    label: String,
    kwh: f64,
}

/// The time range selected in the UI, which also determines the aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Range {
    Day,      // cumulative kWh over the latest 24h
    Week,     // avg kWh/h per day, last 7 days
    Month,    // avg kWh/h per day, last ~30 days
    Year,     // avg kWh/h per month, last 12 months
    Lifetime, // avg kWh/h per month, all data
}

impl Range {
    fn parse(s: &str) -> Range {
        match s.trim().to_ascii_lowercase().as_str() {
            "week" => Range::Week,
            "month" => Range::Month,
            "year" => Range::Year,
            "lifetime" | "all" => Range::Lifetime,
            _ => Range::Day,
        }
    }

    fn chart_kind(self) -> &'static str {
        match self {
            Range::Day => "line",
            _ => "bar",
        }
    }
}

#[derive(Debug, Deserialize)]
struct DataParams {
    range: Option<String>,
}

#[derive(Serialize)]
struct DataResponse {
    chart: &'static str,
    unit: &'static str,
    points: Vec<ChartPoint>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::from_env()?;

    let pool = if config.mock_data {
        tracing::warn!("MOCK_DATA enabled: serving generated data, not querying a database");
        None
    } else {
        Some(
            MySqlPoolOptions::new()
                .max_connections(5)
                .connect(&config.database_url)
                .await
                .context("failed to connect to MySQL database")?,
        )
    };

    let bind_addr = config.bind_addr;
    let state = Arc::new(AppState { pool, config });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/data", get(data))
        .route("/api/config", get(config_info))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;

    tracing::info!("dashboard listening on http://{bind_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

#[derive(Serialize)]
struct ConfigInfo {
    table_name: String,
    time_column: String,
    energy_column: String,
    mock_data: bool,
}

async fn config_info(State(state): State<Arc<AppState>>) -> Json<ConfigInfo> {
    Json(ConfigInfo {
        table_name: state.config.table_name.clone(),
        time_column: state.config.time_column.clone(),
        energy_column: state.config.energy_column.clone(),
        mock_data: state.config.mock_data,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

// ---------------------------------------------------------------------------
// Mock data (no database): a deterministic solar-production model.
// ---------------------------------------------------------------------------

/// Instantaneous power (W) at a given timestamp: a daily sine "bell" between
/// ~06:00 and ~20:00, scaled by a seasonal factor (more in summer) plus a
/// small deterministic variation so it looks realistic but stable on reload.
fn mock_watts(ts: NaiveDateTime) -> f64 {
    let hod = ts.hour() as f64 + ts.minute() as f64 / 60.0;
    if !(6.0..=20.0).contains(&hod) {
        return 0.0;
    }
    let bell = (std::f64::consts::PI * (hod - 6.0) / 14.0).sin().max(0.0);

    // Seasonal factor: peaks near the summer solstice (~day 172), min in winter.
    let doy = ts.ordinal() as f64;
    let seasonal = 0.55 + 0.45 * (2.0 * std::f64::consts::PI * (doy - 172.0) / 365.0).cos();

    // Deterministic "cloud" variation keyed off the day + time.
    let jitter = 0.85 + 0.15 * (0.5 + 0.5 * ((doy + hod) * 1.7).sin());

    (5200.0 * seasonal.max(0.1) * bell.powf(1.6) * jitter).max(0.0)
}

/// Energy (kWh) produced in a single sampling interval at `ts`.
fn mock_interval_kwh(ts: NaiveDateTime, interval_minutes: f64) -> f64 {
    mock_watts(ts) / 1000.0 * (interval_minutes / 60.0)
}

/// A strictly-positive step duration, preserving fractional minutes. Guards the
/// mock loops against a zero-length step (e.g. `INTERVAL_MINUTES=0.5` would
/// truncate to 0 minutes and loop forever).
fn interval_step(interval_minutes: f64) -> Duration {
    Duration::seconds(((interval_minutes * 60.0).round() as i64).max(1))
}

/// Total energy (kWh) produced on a given calendar day.
fn mock_day_kwh(date: NaiveDate, interval_minutes: f64) -> f64 {
    let step = interval_step(interval_minutes);
    let mut t = date.and_hms_opt(0, 0, 0).unwrap();
    let end = date
        .succ_opt()
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let mut sum = 0.0;
    while t < end {
        sum += mock_interval_kwh(t, interval_minutes);
        t += step;
    }
    sum
}

/// Daylight span (hours where power > 0) for a day in the mock model.
fn mock_daylight_hours(date: NaiveDate, interval_minutes: f64) -> f64 {
    let step = interval_step(interval_minutes);
    let mut t = date.and_hms_opt(0, 0, 0).unwrap();
    let end = date.succ_opt().unwrap().and_hms_opt(0, 0, 0).unwrap();
    let (mut first, mut last) = (None, None);
    while t < end {
        if mock_watts(t) > 0.0 {
            first.get_or_insert(t);
            last = Some(t);
        }
        t += step;
    }
    match (first, last) {
        (Some(f), Some(l)) => ((l - f).num_seconds() as f64 / 3600.0).max(0.0167),
        _ => 0.0167,
    }
}

/// Denominator hours for a day under the chosen basis.
fn mock_day_hours(date: NaiveDate, interval_minutes: f64, basis: AverageBasis) -> f64 {
    match basis {
        AverageBasis::FixedDay => 24.0,
        AverageBasis::Daylight => mock_daylight_hours(date, interval_minutes),
    }
}

/// Average kWh per hour for a single day: daily energy / denominator hours.
fn mock_day_avg_per_hour(date: NaiveDate, interval_minutes: f64, basis: AverageBasis) -> f64 {
    mock_day_kwh(date, interval_minutes) / mock_day_hours(date, interval_minutes, basis)
}

/// Average kWh per hour for a month: total month energy / total month hours.
fn mock_month_avg_per_hour(
    year: i32,
    month: u32,
    interval_minutes: f64,
    basis: AverageBasis,
) -> f64 {
    let (mut kwh, mut hours) = (0.0, 0.0);
    let mut d = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    while d.month() == month {
        kwh += mock_day_kwh(d, interval_minutes);
        hours += mock_day_hours(d, interval_minutes, basis);
        d = d.succ_opt().unwrap();
    }
    if hours > 0.0 {
        kwh / hours
    } else {
        0.0
    }
}

/// Builds the chart points for a given range entirely from the mock model.
fn generate_mock(
    range: Range,
    interval_minutes: f64,
    basis: AverageBasis,
    limit: i64,
) -> Vec<ChartPoint> {
    let now = Local::now().naive_local();
    match range {
        // A single calendar day: cumulative kWh from midnight, capped to the
        // most recent `limit` points (not used as a time range).
        Range::Day => {
            let step = interval_step(interval_minutes);
            let start = now.date().and_hms_opt(0, 0, 0).unwrap();
            let end = now.date().succ_opt().unwrap().and_hms_opt(0, 0, 0).unwrap();
            let mut out = Vec::new();
            let mut cum = 0.0;
            let mut t = start;
            while t < end {
                cum += mock_interval_kwh(t, interval_minutes);
                out.push(ChartPoint {
                    label: t.format("%m-%d %H:%M").to_string(),
                    kwh: round2(cum),
                });
                t += step;
            }
            let cap = limit.clamp(1, 5000) as usize;
            if out.len() > cap {
                out.split_off(out.len() - cap)
            } else {
                out
            }
        }
        Range::Week | Range::Month => {
            let days = if range == Range::Week { 7 } else { 30 };
            let today = now.date();
            (0..days)
                .rev()
                .map(|back| {
                    let d = today - Duration::days(back);
                    ChartPoint {
                        label: d.format("%Y-%m-%d").to_string(),
                        kwh: round2(mock_day_avg_per_hour(d, interval_minutes, basis)),
                    }
                })
                .collect()
        }
        Range::Year | Range::Lifetime => {
            let months = if range == Range::Year { 12 } else { 18 };
            let (mut y, mut m) = (now.year(), now.month());
            for _ in 0..(months - 1) {
                if m == 1 {
                    m = 12;
                    y -= 1;
                } else {
                    m -= 1;
                }
            }
            let mut out = Vec::with_capacity(months as usize);
            for _ in 0..months {
                out.push(ChartPoint {
                    label: format!("{y:04}-{m:02}"),
                    kwh: round2(mock_month_avg_per_hour(y, m, interval_minutes, basis)),
                });
                if m == 12 {
                    m = 1;
                    y += 1;
                } else {
                    m += 1;
                }
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Database-backed queries.
// ---------------------------------------------------------------------------

/// SQL expression yielding a real datetime from the configured time column.
/// `invertertime` is stored as an ISO-8601 string with a TZ offset, so we parse
/// the wall-clock portion; a native DATETIME column is used as-is.
fn time_expr(time_col: &str, parse_iso: bool) -> String {
    if parse_iso {
        // Use the first 19 chars (YYYY-MM-DDTHH:MM:SS) so the trailing TZ offset
        // never confuses STR_TO_DATE.
        format!("STR_TO_DATE(LEFT(`{time_col}`, 19), '%Y-%m-%dT%H:%i:%s')")
    } else {
        format!("`{time_col}`")
    }
}

/// Per-day hours used as the denominator for the average, by basis.
fn day_hours_expr(ts: &str, basis: AverageBasis) -> String {
    match basis {
        AverageBasis::FixedDay => "24.0".to_string(),
        // Guard against single-reading days (span 0) with a 1-minute floor.
        AverageBasis::Daylight => {
            format!("GREATEST(TIMESTAMPDIFF(SECOND, MIN({ts}), MAX({ts})) / 3600.0, 0.0167)")
        }
    }
}

/// Builds the SQL for a range.
///
/// * Day  — cumulative kWh (`energy / 1000`) per reading over the latest 24h.
/// * Week/Month — AVERAGE kWh per hour per day: `MAX(energy)/1000 / day_hours`,
///   where `energy` is the daily cumulative counter (resets at 0 each day).
/// * Year/Lifetime — average kWh per hour per month: total monthly energy
///   (`SUM` of daily `MAX`) divided by the month's hours.
///
/// Windows are relative to the newest reading in the table (not `NOW()`), so a
/// static/historical dataset still renders. Identifiers are validated, so
/// interpolation is safe.
fn build_sql(range: Range, table: &str, ts: &str, energy_col: &str, basis: AverageBasis) -> String {
    let e = format!("CAST(`{energy_col}` AS DOUBLE)");
    // Anchor windows to the newest *calendar date/month* in the table, aligned to
    // day/month boundaries so a range yields exactly N full buckets (no partial
    // leading day/month, no spurious extra bucket).
    let latest_date = format!("(SELECT DATE(MAX({ts})) FROM `{table}`)");
    let latest_month = format!("(SELECT DATE_FORMAT(MAX({ts}), '%Y-%m-01') FROM `{table}`)");

    match range {
        // A single calendar day: the latest day's cumulative counter curve.
        Range::Day => format!(
            "SELECT DATE_FORMAT({ts}, '%m-%d %H:%i') AS label, \
                COALESCE({e}, 0) / 1000.0 AS kwh \
             FROM `{table}` \
             WHERE DATE({ts}) = {latest_date} \
             ORDER BY {ts} ASC \
             LIMIT ?"
        ),
        Range::Week | Range::Month => {
            // 7 or 30 whole calendar days ending on the latest date.
            let back = if range == Range::Week { 6 } else { 29 };
            let denom = day_hours_expr(ts, basis);
            format!(
                "SELECT DATE_FORMAT({ts}, '%Y-%m-%d') AS label, \
                    COALESCE(MAX({e}), 0) / 1000.0 / ({denom}) AS kwh \
                 FROM `{table}` \
                 WHERE DATE({ts}) >= ({latest_date} - INTERVAL {back} DAY) \
                 GROUP BY DATE({ts}) \
                 ORDER BY DATE({ts}) ASC"
            )
        }
        Range::Year | Range::Lifetime => {
            // 12 whole calendar months ending on the latest month.
            let where_clause = if range == Range::Year {
                format!("WHERE {ts} >= ({latest_month} - INTERVAL 11 MONTH) ")
            } else {
                String::new()
            };
            // Inner query: one row per day with that day's energy and hours.
            let (day_hours, month_hours) = match basis {
                AverageBasis::FixedDay => ("24.0".to_string(), "COUNT(*) * 24.0".to_string()),
                AverageBasis::Daylight => (
                    format!(
                        "GREATEST(TIMESTAMPDIFF(SECOND, MIN({ts}), MAX({ts})) / 3600.0, 0.0167)"
                    ),
                    "SUM(day_hours)".to_string(),
                ),
            };
            format!(
                "SELECT DATE_FORMAT(d, '%Y-%m') AS label, \
                    SUM(day_energy) / 1000.0 / ({month_hours}) AS kwh \
                 FROM ( \
                    SELECT DATE({ts}) AS d, \
                        MAX({e}) AS day_energy, \
                        {day_hours} AS day_hours \
                    FROM `{table}` {where_clause}\
                    GROUP BY DATE({ts}) \
                 ) t \
                 GROUP BY DATE_FORMAT(d, '%Y-%m') \
                 ORDER BY DATE_FORMAT(d, '%Y-%m') ASC"
            )
        }
    }
}

async fn data(
    State(state): State<Arc<AppState>>,
    Query(params): Query<DataParams>,
) -> Result<Json<DataResponse>, ApiError> {
    let cfg = &state.config;
    let range = params.range.as_deref().map(Range::parse).unwrap_or(Range::Day);
    let limit = cfg.default_limit.clamp(1, 100_000);

    let points = if cfg.mock_data {
        generate_mock(range, cfg.interval_minutes, cfg.average_basis, limit)
    } else {
        let pool = state
            .pool
            .as_ref()
            .ok_or_else(|| ApiError(anyhow!("database not configured")))?;

        let ts = time_expr(&cfg.time_column, cfg.parse_iso_time);
        let sql = build_sql(range, &cfg.table_name, &ts, &cfg.energy_column, cfg.average_basis);
        let mut query = sqlx::query_as::<_, ChartPoint>(&sql);
        if range == Range::Day {
            query = query.bind(limit);
        }
        query
            .fetch_all(pool)
            .await
            .map_err(|e| ApiError(anyhow!(e).context("query failed")))?
    };

    // Day is cumulative energy (kWh); the aggregated views are an hourly average.
    let unit = if range == Range::Day { "kWh" } else { "kWh/h" };

    Ok(Json(DataResponse {
        chart: range.chart_kind(),
        unit,
        points,
    }))
}

/// Converts any error into a 500 JSON response.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("request error: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("{:#}", self.0) })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    // ---- validate_identifier ------------------------------------------------

    #[test]
    fn identifier_accepts_plain_names() {
        for s in ["readings", "invertertime", "col_1", "A1_b2"] {
            assert!(validate_identifier(s).is_ok(), "should accept {s}");
        }
    }

    #[test]
    fn identifier_rejects_injection_and_empty() {
        for s in ["", "a b", "drop;", "tbl-1", "`x`", "a.b", "x'y"] {
            assert!(validate_identifier(s).is_err(), "should reject {s:?}");
        }
    }

    // ---- Range --------------------------------------------------------------

    #[test]
    fn range_parse_known_values() {
        assert_eq!(Range::parse("day"), Range::Day);
        assert_eq!(Range::parse("week"), Range::Week);
        assert_eq!(Range::parse("month"), Range::Month);
        assert_eq!(Range::parse("year"), Range::Year);
        assert_eq!(Range::parse("lifetime"), Range::Lifetime);
        assert_eq!(Range::parse("all"), Range::Lifetime);
    }

    #[test]
    fn range_parse_is_case_insensitive_and_trims() {
        assert_eq!(Range::parse("  WEEK "), Range::Week);
        assert_eq!(Range::parse("Year"), Range::Year);
    }

    #[test]
    fn range_parse_unknown_defaults_to_day() {
        assert_eq!(Range::parse(""), Range::Day);
        assert_eq!(Range::parse("nonsense"), Range::Day);
    }

    #[test]
    fn range_chart_kind() {
        assert_eq!(Range::Day.chart_kind(), "line");
        for r in [Range::Week, Range::Month, Range::Year, Range::Lifetime] {
            assert_eq!(r.chart_kind(), "bar");
        }
    }

    // ---- round2 -------------------------------------------------------------

    #[test]
    fn round2_rounds_to_two_places() {
        assert_eq!(round2(1.234), 1.23);
        assert_eq!(round2(1.235), 1.24);
        assert_eq!(round2(0.0), 0.0);
        assert_eq!(round2(13.0), 13.0);
    }

    // ---- AverageBasis -------------------------------------------------------

    #[test]
    fn average_basis_parse() {
        assert_eq!(AverageBasis::parse("daylight"), AverageBasis::Daylight);
        assert_eq!(AverageBasis::parse("DAY"), AverageBasis::FixedDay);
        assert_eq!(AverageBasis::parse(""), AverageBasis::FixedDay);
        assert_eq!(AverageBasis::parse("anything"), AverageBasis::FixedDay);
    }

    // ---- time_expr ----------------------------------------------------------

    #[test]
    fn time_expr_parses_iso_or_uses_column() {
        assert_eq!(
            time_expr("invertertime", true),
            "STR_TO_DATE(LEFT(`invertertime`, 19), '%Y-%m-%dT%H:%i:%s')"
        );
        assert_eq!(time_expr("created_at", false), "`created_at`");
    }

    // ---- build_sql ----------------------------------------------------------

    fn sql(range: Range, basis: AverageBasis) -> String {
        let ts = time_expr("invertertime", true);
        build_sql(range, "solarpower", &ts, "wattsperhour", basis)
    }

    #[test]
    fn sql_reads_energy_column_not_watts() {
        for r in [Range::Day, Range::Week, Range::Month, Range::Year, Range::Lifetime] {
            let s = sql(r, AverageBasis::FixedDay);
            assert!(s.contains("wattsperhour"), "{r:?} should use energy column");
            assert!(!s.contains("`watts`"), "{r:?} must not use the instantaneous watts column");
        }
    }

    #[test]
    fn sql_interpolates_table_and_energy_and_time() {
        let s = sql(Range::Week, AverageBasis::FixedDay);
        assert!(s.contains("`solarpower`"));
        assert!(s.contains("`wattsperhour`"));
        assert!(s.contains("`invertertime`"));
        assert!(s.contains("STR_TO_DATE(LEFT("));
    }

    #[test]
    fn sql_windows_relative_to_latest_reading_not_now() {
        // All non-lifetime ranges base their window on MAX(ts), not NOW().
        for r in [Range::Day, Range::Week, Range::Month, Range::Year] {
            let s = sql(r, AverageBasis::FixedDay);
            assert!(s.contains("MAX("), "{r:?} should window off the latest reading");
            assert!(!s.contains("NOW()"), "{r:?} must not use NOW()");
        }
    }

    #[test]
    fn sql_day_is_cumulative_with_limit() {
        let s = sql(Range::Day, AverageBasis::FixedDay);
        assert!(s.contains("WHERE DATE("), "day view is a single calendar day");
        assert!(s.contains("= (SELECT DATE(MAX("), "pinned to the latest date");
        assert!(s.contains("LIMIT ?"));
        assert!(s.contains("/ 1000.0"));
        assert!(!s.contains("GROUP BY"), "day view is per-reading, not aggregated");
    }

    #[test]
    fn sql_week_and_month_use_daily_max() {
        let week = sql(Range::Week, AverageBasis::FixedDay);
        assert!(week.contains("INTERVAL 6 DAY"), "7 whole calendar days");
        assert!(week.contains("GROUP BY DATE("));
        assert!(week.contains("MAX("), "daily production is the counter MAX");
        assert!(!week.contains("AVG("), "should not average the cumulative counter");

        let month = sql(Range::Month, AverageBasis::FixedDay);
        assert!(month.contains("INTERVAL 29 DAY"), "30 whole calendar days");
    }

    #[test]
    fn sql_basis_controls_denominator() {
        let fixed = sql(Range::Week, AverageBasis::FixedDay);
        assert!(fixed.contains("/ (24.0)"), "fixed basis divides by 24");
        assert!(!fixed.contains("TIMESTAMPDIFF"));

        let daylight = sql(Range::Week, AverageBasis::Daylight);
        assert!(daylight.contains("TIMESTAMPDIFF"), "daylight basis divides by the reading span");
    }

    #[test]
    fn sql_year_windows_one_year_and_groups_by_month() {
        let s = sql(Range::Year, AverageBasis::FixedDay);
        assert!(s.contains("INTERVAL 11 MONTH"), "12 whole calendar months");
        assert!(s.contains("DATE_FORMAT(d, '%Y-%m')"));
        assert!(s.contains("MAX(") && s.contains("SUM("), "month = sum of daily maxes");
    }

    #[test]
    fn sql_lifetime_has_no_time_filter() {
        let s = sql(Range::Lifetime, AverageBasis::FixedDay);
        assert!(!s.contains("WHERE"), "lifetime should scan all rows");
        assert!(s.contains("DATE_FORMAT(d, '%Y-%m')"));
    }

    // ---- mock model ---------------------------------------------------------

    #[test]
    fn mock_watts_zero_overnight_positive_midday() {
        assert_eq!(mock_watts(dt(2026, 6, 21, 2, 0)), 0.0);
        assert_eq!(mock_watts(dt(2026, 6, 21, 23, 0)), 0.0);
        assert!(mock_watts(dt(2026, 6, 21, 12, 0)) > 0.0);
    }

    #[test]
    fn mock_watts_summer_exceeds_winter_at_noon() {
        let summer = mock_watts(dt(2026, 6, 21, 12, 0));
        let winter = mock_watts(dt(2026, 12, 21, 12, 0));
        assert!(summer > winter, "summer {summer} should beat winter {winter}");
    }

    #[test]
    fn mock_interval_kwh_scales_with_interval() {
        let ts = dt(2026, 6, 21, 12, 0);
        let fifteen = mock_interval_kwh(ts, 15.0);
        let thirty = mock_interval_kwh(ts, 30.0);
        assert!((thirty - 2.0 * fifteen).abs() < 1e-9);
    }

    #[test]
    fn mock_day_kwh_is_positive() {
        assert!(mock_day_kwh(NaiveDate::from_ymd_opt(2026, 6, 21).unwrap(), 15.0) > 0.0);
    }

    const B: AverageBasis = AverageBasis::FixedDay;

    #[test]
    fn generate_mock_point_counts_per_range() {
        assert_eq!(generate_mock(Range::Day, 15.0, B, 96).len(), 96);
        assert_eq!(generate_mock(Range::Week, 15.0, B, 1000).len(), 7);
        assert_eq!(generate_mock(Range::Month, 15.0, B, 1000).len(), 30);
        assert_eq!(generate_mock(Range::Year, 15.0, B, 1000).len(), 12);
        assert_eq!(generate_mock(Range::Lifetime, 15.0, B, 1000).len(), 18);
    }

    #[test]
    fn generate_mock_day_is_one_full_calendar_day() {
        // Full day at 15-min cadence = 96 points, regardless of a huge cap.
        assert_eq!(generate_mock(Range::Day, 15.0, B, 10_000).len(), 96);
        // Cap keeps only the most recent `limit` points.
        assert_eq!(generate_mock(Range::Day, 15.0, B, 0).len(), 1);
        assert_eq!(generate_mock(Range::Day, 15.0, B, 10).len(), 10);
        // Every point is on the same calendar day (shared MM-DD prefix).
        let pts = generate_mock(Range::Day, 15.0, B, 10_000);
        let first_day = pts[0].label[..5].to_string();
        assert!(pts.iter().all(|p| p.label[..5] == first_day));
    }

    #[test]
    fn interval_step_is_always_positive() {
        assert_eq!(interval_step(0.5), Duration::seconds(30));
        assert_eq!(interval_step(15.0), Duration::seconds(900));
        assert!(interval_step(0.0) >= Duration::seconds(1));
        assert!(interval_step(0.001) >= Duration::seconds(1));
    }

    #[test]
    fn generate_mock_handles_fractional_interval_without_hanging() {
        // A sub-minute interval must still terminate and respect the cap.
        assert_eq!(generate_mock(Range::Day, 0.5, B, 50).len(), 50);
        assert!(!generate_mock(Range::Week, 0.5, B, 1000).is_empty());
    }

    #[test]
    fn generate_mock_values_are_finite_and_nonnegative() {
        for basis in [AverageBasis::FixedDay, AverageBasis::Daylight] {
            for r in [Range::Day, Range::Week, Range::Month, Range::Year, Range::Lifetime] {
                for p in generate_mock(r, 15.0, basis, 1000) {
                    assert!(p.kwh.is_finite() && p.kwh >= 0.0, "{r:?}/{basis:?} -> {p:?}");
                    assert!(!p.label.is_empty());
                }
            }
        }
    }

    #[test]
    fn generate_mock_day_is_cumulative_nondecreasing_within_day() {
        // Cumulative kWh must never decrease within a single day.
        let pts = generate_mock(Range::Day, 15.0, B, 96);
        for w in pts.windows(2) {
            // same day (labels share the "MM-DD" prefix) => non-decreasing
            if w[0].label[..5] == w[1].label[..5] {
                assert!(w[1].kwh >= w[0].kwh, "cumulative dropped: {:?} -> {:?}", w[0], w[1]);
            }
        }
    }

    #[test]
    fn generate_mock_daylight_basis_exceeds_fixed_for_aggregates() {
        // Dividing by ~daylight hours (<24) yields a larger avg than /24.
        let fixed: f64 = generate_mock(Range::Week, 15.0, AverageBasis::FixedDay, 1000)
            .iter().map(|p| p.kwh).sum();
        let daylight: f64 = generate_mock(Range::Week, 15.0, AverageBasis::Daylight, 1000)
            .iter().map(|p| p.kwh).sum();
        assert!(daylight > fixed, "daylight avg {daylight} should exceed fixed-24 avg {fixed}");
    }

    #[test]
    fn generate_mock_month_labels_are_year_month() {
        for p in generate_mock(Range::Lifetime, 15.0, B, 1000) {
            assert_eq!(p.label.len(), 7, "expected YYYY-MM, got {}", p.label);
            assert_eq!(p.label.as_bytes()[4], b'-');
        }
    }
}
