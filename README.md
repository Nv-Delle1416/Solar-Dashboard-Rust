# Solar Energy Dashboard

A small Rust web app that queries a MySQL/MariaDB table and visualizes solar
**energy production in kWh** in the browser. Built with
[axum](https://github.com/tokio-rs/axum),
[sqlx](https://github.com/launchbadge/sqlx), and [Chart.js](https://www.chartjs.org/).

Pick a time range at the top of the page:

| range      | what it shows                                   |
|------------|-------------------------------------------------|
| Day        | every reading over the last 24h (kWh per interval) |
| Week       | total kWh produced per day, last 7 days         |
| Month      | total kWh produced per day, last 30 days        |
| Year       | total kWh produced per month, last 12 months    |
| Lifetime   | total kWh produced per month, all time          |

Power readings (`Watts`) are converted to energy with
`kWh = Watts / 1000 * (INTERVAL_MINUTES / 60)`.

The table is expected to have these columns:

| column        | meaning                          |
|---------------|----------------------------------|
| `id`          | row id                           |
| `Watts`       | instantaneous power              |
| `wattsperhour`| energy per hour                  |
| `invertertime`| timestamp from the inverter      |
| `created_at`  | row insertion time               |

## Configuration

Everything is configured through environment variables (a `.env` file is loaded
automatically). Copy the example and edit it:

```sh
cp .env.example .env
```

| variable        | default              | description                                        |
|-----------------|----------------------|----------------------------------------------------|
| `MOCK_DATA`     | `false`              | serve generated data with no DB (ignores `DATABASE_URL`) |
| `DATABASE_URL`  | *(required)*         | MySQL endpoint: `mysql://user:pass@host:3306/db` (not needed in mock mode) |
| `TABLE_NAME`    | `readings`           | table to query                                     |
| `TIME_COLUMN`   | `invertertime`       | column used for the x-axis / ordering              |
| `BIND_ADDR`     | `127.0.0.1:8080`     | address the web server listens on                  |
| `DEFAULT_LIMIT` | `1000`               | max rows returned for the Day view (safety cap)    |
| `INTERVAL_MINUTES` | `15`              | minutes between readings; used for the kWh conversion |
| `RUST_LOG`      | `info`               | log verbosity                                      |

> **Note:** `TABLE_NAME` and `TIME_COLUMN` are validated to be plain SQL
> identifiers (letters, digits, underscore) to prevent injection, since
> identifiers cannot be passed as bound query parameters.

## Mock data (no database)

The quickest way to preview the dashboard is mock mode: the server generates a
realistic solar-production curve in-memory and never connects to a database.

```sh
MOCK_DATA=true cargo run
```

(On Windows PowerShell: `$env:MOCK_DATA="true"; cargo run`. Or just set
`MOCK_DATA=true` in your `.env`.)

The UI header will show "Mock data (no database)" so it's obvious the chart
isn't backed by real rows.

### Mock data in a real database

If you'd rather load sample rows into an actual MySQL database, use the seed
script. It creates the `readings` table and inserts a solar curve for the last
3 days at 15-minute intervals:

```sh
mysql -u <user> -p <database> < seed.sql
```

(Requires MySQL 8.0+ or MariaDB 10.2+. Edit `seed.sql` to change the date range
or interval.)

## Running

```sh
cargo run
```

Then open the URL printed in the logs (default <http://127.0.0.1:8080>).

## Endpoints

- `GET /` – the dashboard UI
- `GET /api/data?range=day|week|month|year|lifetime` – returns
  `{ "chart": "line|bar", "unit": "kWh", "points": [{ "label", "kwh" }] }`
- `GET /api/config` – reports the active table, time column, and mock flag

## Build a release binary

```sh
cargo build --release
./target/release/watts-dashboard
```
