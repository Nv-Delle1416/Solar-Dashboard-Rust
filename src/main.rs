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

/// Builds the SQL for a range. `factor` converts a watt(-sum) into kWh:
/// energy = Watts / 1000 * (interval_minutes / 60). Table/time-column names are
/// validated identifiers (not bindable), so interpolation is safe here.
fn build_sql(range: Range, table: &str, time_col: &str) -> String {
    let watts = "CAST(Watts AS DOUBLE)";
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
    let factor = cfg.interval_minutes / 60.0 / 1000.0;

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
