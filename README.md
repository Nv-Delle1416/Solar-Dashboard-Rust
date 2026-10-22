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

## Database connection & authentication

Database authentication is handled entirely through the `DATABASE_URL`
connection string — there is **no interactive password prompt**. Whatever
credentials are in the URL are what the app uses to authenticate against
MySQL/MariaDB. The URL format is:

```
mysql://USER:PASSWORD@HOST:PORT/DATABASE
```

For example:

```
mysql://solar_ro:s3cret@db.example.com:3306/solar
```

### Passwords with special characters

If your password contains URL-reserved characters (`@ : / ? # [ ] %` …) you
**must percent-encode them**, otherwise the URL parses incorrectly. For example,
the password `p@ss:word/1` becomes:

```
mysql://user:p%40ss%3Aword%2F1@host:3306/db
```

Common encodings: `@` → `%40`, `:` → `%3A`, `/` → `%2F`, `#` → `%23`,
`?` → `%3F`, `%` → `%25`.

### Authentication plugins

Both MySQL authentication plugins are supported out of the box:

- `mysql_native_password`
- `caching_sha2_password` (the default on MySQL 8+) — this also works over a
  non-TLS connection via RSA key exchange.

### TLS / encrypted connections

The binary is built with TLS support (`tls-rustls`). To require an encrypted
connection, add an `ssl-mode` query parameter:

```
mysql://user:pass@host:3306/db?ssl-mode=REQUIRED
```

### What happens on an authentication failure

The database connection is established **once at startup**. If the credentials
are rejected (or the host is unreachable), the app does **not** retry and does
**not** start the web server — it exits immediately with an error such as:

```
Error: failed to connect to MySQL database

Caused by:
    Access denied for user 'user'@'host' (using password: YES)
```

Fix the credentials in `DATABASE_URL` and start the app again. (In
[mock mode](#mock-data-no-database) the database is skipped entirely, so no
credentials are needed.)

### Keeping credentials out of version control

`DATABASE_URL` is read from the environment or a local `.env` file. The `.env`
file is listed in `.gitignore`, so your real credentials are never committed —
only `.env.example` (which contains placeholders) is tracked.

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
