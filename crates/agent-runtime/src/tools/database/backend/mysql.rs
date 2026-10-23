//! MySQL / MariaDB backend (MySQL, Cloud SQL-MySQL).
//!
//! Reads run inside a `START TRANSACTION READ ONLY` with a `max_execution_time`
//! ceiling and stream row-by-row so the export path never materializes the full
//! result. A wall-clock timeout backstops every read (covering MariaDB, where
//! the timeout variable differs), and any error/early-stop discards the
//! connection rather than returning a dirty one to the pool. `mysql_async`'s
//! [`MyValue`] enum decodes directly; text/decimal bytes route through the
//! shared [`text_to_json`] so numeric typing matches the Postgres backend.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use eyre::{eyre, Result};
use futures::StreamExt;
use mysql_async::prelude::Queryable;
use mysql_async::{Opts, OptsBuilder, Pool, Row, SslOpts, Value as MyValue};
use serde_json::{json, Value};

use super::{text_to_json, Backend, Engine, ExecOutcome, Flow, RowSink};

const MY_SYSTEM_SCHEMAS: &[&str] = &["information_schema", "performance_schema", "mysql", "sys"];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_SLACK: Duration = Duration::from_secs(5);

pub struct MyBackend {
    pool: Pool,
    allow_writes: bool,
}

impl MyBackend {
    /// Build a lazily-connecting pool. With `ssl`, TLS is used; certificate
    /// verification is skipped only when the instance is left at the default
    /// `tls_insecure` posture (managed endpoints present private-CA certs).
    pub fn new(dsn: &str, allow_writes: bool, ssl: bool, tls_insecure: bool) -> Result<Self> {
        let base =
            Opts::from_url(dsn).map_err(|e| eyre!("invalid MySQL connection string: {e}"))?;
        let mut builder = OptsBuilder::from_opts(base);
        if ssl {
            let mut ssl_opts = SslOpts::default();
            if tls_insecure {
                ssl_opts = ssl_opts
                    .with_danger_accept_invalid_certs(true)
                    .with_danger_skip_domain_validation(true);
            }
            builder = builder.ssl_opts(ssl_opts);
        }
        Ok(Self {
            pool: Pool::new(Opts::from(builder)),
            allow_writes,
        })
    }

    async fn conn(&self) -> Result<mysql_async::Conn> {
        match tokio::time::timeout(CONNECT_TIMEOUT, self.pool.get_conn()).await {
            Ok(Ok(c)) => Ok(c),
            Ok(Err(e)) => Err(eyre!("failed to acquire a MySQL connection: {e}")),
            Err(_) => Err(eyre!("timed out connecting to the database")),
        }
    }
}

fn pad2(n: u8) -> String {
    format!("{n:02}")
}

fn micros_suffix(us: u32) -> String {
    if us == 0 {
        String::new()
    } else {
        format!(".{us:06}")
    }
}

fn value_to_json(value: &MyValue) -> Value {
    match value {
        MyValue::NULL => Value::Null,
        MyValue::Int(i) => json!(i),
        MyValue::UInt(u) => json!(u),
        MyValue::Float(f) => {
            serde_json::Number::from_f64(*f as f64).map_or(Value::Null, Value::Number)
        }
        MyValue::Double(d) => serde_json::Number::from_f64(*d).map_or(Value::Null, Value::Number),
        MyValue::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => text_to_json(Some(s)),
            Err(_) => Value::String(base64::engine::general_purpose::STANDARD.encode(b)),
        },
        MyValue::Date(y, mo, d, h, mi, s, us) => {
            let date = format!("{y:04}-{}-{}", pad2(*mo), pad2(*d));
            if *h == 0 && *mi == 0 && *s == 0 && *us == 0 {
                Value::String(date)
            } else {
                Value::String(format!(
                    "{date} {}:{}:{}{}",
                    pad2(*h),
                    pad2(*mi),
                    pad2(*s),
                    micros_suffix(*us)
                ))
            }
        }
        MyValue::Time(neg, d, h, mi, s, us) => {
            let hours = *d * 24 + u32::from(*h);
            Value::String(format!(
                "{}{:02}:{}:{}{}",
                if *neg { "-" } else { "" },
                hours,
                pad2(*mi),
                pad2(*s),
                micros_suffix(*us)
            ))
        }
    }
}

fn row_cells(row: &Row) -> Vec<Value> {
    (0..row.len())
        .map(|i| row.as_ref(i).map_or(Value::Null, value_to_json))
        .collect()
}

#[async_trait]
impl Backend for MyBackend {
    fn engine(&self) -> Engine {
        Engine::Mysql
    }

    fn allow_writes(&self) -> bool {
        self.allow_writes
    }

    async fn ping(&self) -> Result<()> {
        self.conn().await?.query_drop("SELECT 1").await?;
        Ok(())
    }

    async fn export(
        &self,
        sql: &str,
        timeout: Duration,
        sink: &mut (dyn RowSink + Send),
    ) -> Result<usize> {
        let mut conn = self.conn().await?;
        conn.query_drop("START TRANSACTION READ ONLY").await?;
        conn.query_drop(format!(
            "SET SESSION max_execution_time = {}",
            timeout.as_millis()
        ))
        .await
        .ok();

        let read = async {
            let mut result = conn.query_iter(sql).await?;
            let columns: Vec<String> = result
                .columns()
                .map(|cols| cols.iter().map(|c| c.name_str().to_string()).collect())
                .unwrap_or_default();
            sink.columns(&columns)?;
            let mut total = 0usize;
            let mut stopped = false;
            if let Some(mut stream) = result.stream::<Row>().await? {
                while let Some(row) = stream.next().await {
                    let row = row?;
                    total += 1;
                    if sink.push(row_cells(&row))? == Flow::Stop {
                        stopped = true;
                        break;
                    }
                }
            }
            // Drop the result set (releasing the borrow on `conn`) before the
            // caller reuses or discards the connection.
            drop(result);
            Ok::<(usize, bool), eyre::Report>((total, stopped))
        };

        // Wall-clock backstop: the engine-agnostic guarantee a slow read can't
        // hang the turn (MariaDB ignores `max_execution_time`).
        match tokio::time::timeout(timeout + CLEANUP_SLACK, read).await {
            Ok(Ok((total, stopped))) => {
                if stopped {
                    conn.disconnect().await.ok();
                } else {
                    conn.query_drop("COMMIT").await.ok();
                }
                Ok(total)
            }
            Ok(Err(e)) => {
                conn.disconnect().await.ok();
                Err(e)
            }
            Err(_) => {
                conn.disconnect().await.ok();
                Err(eyre!(
                    "query exceeded the {}ms statement timeout",
                    timeout.as_millis()
                ))
            }
        }
    }

    async fn execute(&self, sql: &str, timeout: Duration) -> Result<ExecOutcome> {
        let mut conn = self.conn().await?;
        match tokio::time::timeout(timeout + CLEANUP_SLACK, conn.query_drop(sql)).await {
            Ok(Ok(())) => Ok(ExecOutcome {
                rows_affected: conn.affected_rows(),
                statements: 1,
            }),
            Ok(Err(e)) => {
                conn.disconnect().await.ok();
                Err(e.into())
            }
            Err(_) => {
                conn.disconnect().await.ok();
                Err(eyre!(
                    "statement exceeded the {}ms timeout",
                    timeout.as_millis()
                ))
            }
        }
    }

    fn system_schemas(&self) -> &'static [&'static str] {
        MY_SYSTEM_SCHEMAS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_common_value_kinds() {
        assert_eq!(value_to_json(&MyValue::NULL), Value::Null);
        assert_eq!(value_to_json(&MyValue::Int(-9)), json!(-9));
        assert_eq!(value_to_json(&MyValue::UInt(9)), json!(9));
        assert_eq!(value_to_json(&MyValue::Double(1.5)), json!(1.5));
        // Decimal/text arrives as Bytes; numbers round-trip, codes stay strings.
        assert_eq!(
            value_to_json(&MyValue::Bytes(b"123.45".to_vec())),
            json!(123.45)
        );
        assert_eq!(
            value_to_json(&MyValue::Bytes(b"007".to_vec())),
            json!("007")
        );
        assert_eq!(
            value_to_json(&MyValue::Bytes(b"hello".to_vec())),
            json!("hello")
        );
        assert_eq!(
            value_to_json(&MyValue::Date(2026, 6, 14, 0, 0, 0, 0)),
            json!("2026-06-14")
        );
        assert_eq!(
            value_to_json(&MyValue::Date(2026, 6, 14, 9, 5, 3, 0)),
            json!("2026-06-14 09:05:03")
        );
        assert_eq!(
            value_to_json(&MyValue::Time(true, 0, 2, 3, 4, 0)),
            json!("-02:03:04")
        );
    }

    #[test]
    fn binary_bytes_become_base64() {
        let v = value_to_json(&MyValue::Bytes(vec![0xff, 0xfe, 0x00]));
        assert!(matches!(v, Value::String(_)));
    }
}
