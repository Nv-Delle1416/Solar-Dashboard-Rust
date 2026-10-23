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

/// Runtime configuration, loaded from environment variables (see `.env.example`).
#[derive(Clone)]
struct Config {
    database_url: String,
    table_name: String,
    time_column: String,
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

        let table_name = std::env::var("TABLE_NAME").unwrap_or_else(|_| "readings".to_string());
        validate_identifier(&table_name).context("invalid TABLE_NAME")?;

        let time_column =
            std::env::var("TIME_COLUMN").unwrap_or_else(|_| "invertertime".to_string());
        validate_identifier(&time_column).context("invalid TIME_COLUMN")?;

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
    Day,      // all intervals over the last 24h
    Week,     // total kWh per day, last 7 days
    Month,    // total kWh per day, last ~30 days
    Year,     // total kWh per month, last 12 months
    Lifetime, // total kWh per month, all data
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
    mock_data: bool,
}

async fn config_info(State(state): State<Arc<AppState>>) -> Json<ConfigInfo> {
    Json(ConfigInfo {
        table_name: state.config.table_name.clone(),
        time_column: state.config.time_column.clone(),
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

/// Total energy (kWh) produced on a given calendar day.
fn mock_day_kwh(date: NaiveDate, interval_minutes: f64) -> f64 {
    let step = Duration::minutes(interval_minutes as i64);
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

/// Total energy (kWh) produced in a given month.
fn mock_month_kwh(year: i32, month: u32, interval_minutes: f64) -> f64 {
    let mut sum = 0.0;
    let mut d = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    while d.month() == month {
        sum += mock_day_kwh(d, interval_minutes);
        d = d.succ_opt().unwrap();
    }
    sum
}

/// Builds the chart points for a given range entirely from the mock model.
fn generate_mock(range: Range, interval_minutes: f64, limit: i64) -> Vec<ChartPoint> {
    let now = Local::now().naive_local();
    match range {
        Range::Day => {
            let n = limit.clamp(1, 5000);
            let step = Duration::minutes(interval_minutes as i64);
            (0..n)
                .map(|i| {
                    let ts = now - step * ((n - 1 - i) as i32);
                    ChartPoint {
                        label: ts.format("%H:%M").to_string(),
                        kwh: round2(mock_interval_kwh(ts, interval_minutes)),
                    }
                })
                .collect()
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
                        kwh: round2(mock_day_kwh(d, interval_minutes)),
                    }
                })
                .collect()
        }
        Range::Year | Range::Lifetime => {
            let months = if range == Range::Year { 12 } else { 18 };
            let (mut y, mut m) = (now.year(), now.month());
            // Walk back `months - 1` months to find the start.
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
                    kwh: round2(mock_month_kwh(y, m, interval_minutes)),
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

/// Builds the SQL for a range. Energy comes from the `wattsperhour` column
/// (watt-hours), converted to kWh by the bound `factor` (= 1/1000). Table and
/// time-column names are validated identifiers (not bindable), so interpolation
/// is safe here.
fn build_sql(range: Range, table: &str, time_col: &str) -> String {
    let watts = "CAST(wattsperhour AS DOUBLE)";
    match range {
        Range::Day => format!(
            "SELECT DATE_FORMAT(`{time_col}`, '%H:%i') AS label, \
                COALESCE({watts}, 0) * ? AS kwh \
             FROM `{table}` \
             WHERE `{time_col}` >= (NOW() - INTERVAL 1 DAY) \
             ORDER BY `{time_col}` ASC \
             LIMIT ?"
        ),
        Range::Week | Range::Month => {
            let interval = if range == Range::Week { "7 DAY" } else { "30 DAY" };
            format!(
                "SELECT DATE_FORMAT(`{time_col}`, '%Y-%m-%d') AS label, \
                    COALESCE(SUM({watts}), 0) * ? AS kwh \
                 FROM `{table}` \
                 WHERE `{time_col}` >= (NOW() - INTERVAL {interval}) \
                 GROUP BY DATE(`{time_col}`) \
                 ORDER BY DATE(`{time_col}`) ASC"
            )
        }
        Range::Year => format!(
            "SELECT DATE_FORMAT(`{time_col}`, '%Y-%m') AS label, \
                COALESCE(SUM({watts}), 0) * ? AS kwh \
             FROM `{table}` \
             WHERE `{time_col}` >= (NOW() - INTERVAL 1 YEAR) \
             GROUP BY YEAR(`{time_col}`), MONTH(`{time_col}`) \
             ORDER BY YEAR(`{time_col}`), MONTH(`{time_col}`) ASC"
        ),
        Range::Lifetime => format!(
            "SELECT DATE_FORMAT(`{time_col}`, '%Y-%m') AS label, \
                COALESCE(SUM({watts}), 0) * ? AS kwh \
             FROM `{table}` \
             GROUP BY YEAR(`{time_col}`), MONTH(`{time_col}`) \
             ORDER BY YEAR(`{time_col}`), MONTH(`{time_col}`) ASC"
        ),
    }
}

async fn data(
    State(state): State<Arc<AppState>>,
    Query(params): Query<DataParams>,
) -> Result<Json<DataResponse>, ApiError> {
    let cfg = &state.config;
    let range = params.range.as_deref().map(Range::parse).unwrap_or(Range::Day);
    let limit = cfg.default_limit.clamp(1, 100_000);
    // wattsperhour is in watt-hours; divide by 1000 to get kWh.
    let factor = 1.0 / 1000.0;

    let points = if cfg.mock_data {
        generate_mock(range, cfg.interval_minutes, limit)
    } else {
        let pool = state
            .pool
            .as_ref()
            .ok_or_else(|| ApiError(anyhow!("database not configured")))?;

        let sql = build_sql(range, &cfg.table_name, &cfg.time_column);
        let mut query = sqlx::query_as::<_, ChartPoint>(&sql).bind(factor);
        if range == Range::Day {
            query = query.bind(limit);
        }
        query
            .fetch_all(pool)
            .await
            .map_err(|e| ApiError(anyhow!(e).context("query failed")))?
    };

    Ok(Json(DataResponse {
        chart: range.chart_kind(),
        unit: "kWh",
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

    // ---- build_sql ----------------------------------------------------------

    #[test]
    fn sql_reads_wattsperhour_not_watts() {
        for r in [Range::Day, Range::Week, Range::Month, Range::Year, Range::Lifetime] {
            let sql = build_sql(r, "readings", "invertertime");
            assert!(sql.contains("wattsperhour"), "{r:?} should use wattsperhour");
            assert!(!sql.contains("CAST(Watts"), "{r:?} must not use the Watts column");
        }
    }

    #[test]
    fn sql_interpolates_table_and_time_column() {
        let sql = build_sql(Range::Week, "my_table", "ts_col");
        assert!(sql.contains("`my_table`"));
        assert!(sql.contains("`ts_col`"));
    }

    #[test]
    fn sql_day_is_raw_with_limit() {
        let sql = build_sql(Range::Day, "readings", "invertertime");
        assert!(sql.contains("INTERVAL 1 DAY"));
        assert!(sql.contains("LIMIT ?"));
        assert!(!sql.contains("GROUP BY"), "day view should not aggregate");
    }

    #[test]
    fn sql_week_and_month_group_by_day_with_window() {
        let week = build_sql(Range::Week, "readings", "invertertime");
        assert!(week.contains("INTERVAL 7 DAY"));
        assert!(week.contains("GROUP BY DATE(`invertertime`)"));
        assert!(week.contains("SUM("));

        let month = build_sql(Range::Month, "readings", "invertertime");
        assert!(month.contains("INTERVAL 30 DAY"));
    }

    #[test]
    fn sql_year_windows_one_year_and_groups_by_month() {
        let sql = build_sql(Range::Year, "readings", "invertertime");
        assert!(sql.contains("INTERVAL 1 YEAR"));
        assert!(sql.contains("MONTH(`invertertime`)"));
    }

    #[test]
    fn sql_lifetime_has_no_time_filter() {
        let sql = build_sql(Range::Lifetime, "readings", "invertertime");
        assert!(!sql.contains("WHERE"), "lifetime should scan all rows");
        assert!(sql.contains("GROUP BY YEAR"));
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

    #[test]
    fn generate_mock_point_counts_per_range() {
        assert_eq!(generate_mock(Range::Day, 15.0, 96).len(), 96);
        assert_eq!(generate_mock(Range::Week, 15.0, 1000).len(), 7);
        assert_eq!(generate_mock(Range::Month, 15.0, 1000).len(), 30);
        assert_eq!(generate_mock(Range::Year, 15.0, 1000).len(), 12);
        assert_eq!(generate_mock(Range::Lifetime, 15.0, 1000).len(), 18);
    }

    #[test]
    fn generate_mock_day_limit_is_clamped() {
        assert_eq!(generate_mock(Range::Day, 15.0, 0).len(), 1);
        assert_eq!(generate_mock(Range::Day, 15.0, 10_000).len(), 5000);
    }

    #[test]
    fn generate_mock_values_are_finite_and_nonnegative() {
        for r in [Range::Day, Range::Week, Range::Month, Range::Year, Range::Lifetime] {
            for p in generate_mock(r, 15.0, 1000) {
                assert!(p.kwh.is_finite() && p.kwh >= 0.0, "{r:?} -> {p:?}");
                assert!(!p.label.is_empty());
            }
        }
    }

    #[test]
    fn generate_mock_day_labels_are_hh_mm() {
        for p in generate_mock(Range::Day, 15.0, 96) {
            assert_eq!(p.label.len(), 5, "expected HH:MM, got {}", p.label);
            assert_eq!(p.label.as_bytes()[2], b':');
        }
    }

    #[test]
    fn generate_mock_month_labels_are_year_month() {
        for p in generate_mock(Range::Lifetime, 15.0, 1000) {
            assert_eq!(p.label.len(), 7, "expected YYYY-MM, got {}", p.label);
            assert_eq!(p.label.as_bytes()[4], b'-');
        }
    }
}
