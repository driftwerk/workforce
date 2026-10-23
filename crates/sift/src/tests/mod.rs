//! Whole-program tests against a mock host.

mod artifacts;
mod budgets;
mod concurrency;
mod end_to_end;
mod errors;
mod library;
mod limits;
mod sql;
mod syntax;

use crate::{run, Diagnostic, Host, Limits, Outcome};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

tokio::task_local! {
    /// Stands in for the turn's identity, which the real host reads from
    /// task-locals when it authorises a call.
    static TURN: &'static str;
}

type ToolFn = Box<dyn Fn(&Value) -> Result<Value, String> + Send + Sync>;
type DelayFn = Box<dyn Fn(&Value) -> Duration + Send + Sync>;
type SqlFn = Box<dyn Fn(&Value, &str, &[(String, Value)]) -> Result<Value, String> + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct SqlCall {
    pub db: Value,
    pub query: String,
    pub params: Vec<(String, Value)>,
}

pub(super) struct Mock {
    tools: HashMap<String, ToolFn>,
    hidden: Vec<String>,
    sql: Option<SqlFn>,
    delay: DelayFn,
    pub calls: Mutex<Vec<(String, Value)>>,
    pub sql_calls: Mutex<Vec<SqlCall>>,
    in_flight: AtomicUsize,
    pub peak: AtomicUsize,
    /// Tables `artifact_read` can find, by id.
    pub artifacts: HashMap<String, Value>,
    /// Every `artifact_write`: filename and rows.
    pub written: Mutex<Vec<(String, Value)>>,
}

impl Mock {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            hidden: Vec::new(),
            sql: None,
            delay: Box::new(|_| Duration::ZERO),
            calls: Mutex::new(Vec::new()),
            sql_calls: Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            artifacts: HashMap::new(),
            written: Mutex::new(Vec::new()),
        }
    }

    pub fn artifact(mut self, id: &str, rows: Value) -> Self {
        self.artifacts.insert(id.to_string(), rows);
        self
    }

    pub fn tool(
        mut self,
        name: &str,
        f: impl Fn(&Value) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        self.tools.insert(name.to_string(), Box::new(f));
        self
    }

    /// A tool that exists but this program may not call.
    pub fn hidden(mut self, name: &str) -> Self {
        self.hidden.push(name.to_string());
        self.tool(name, |_| Ok(Value::Null))
    }

    pub fn sql(
        mut self,
        f: impl Fn(&Value, &str, &[(String, Value)]) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        self.sql = Some(Box::new(f));
        self
    }

    pub fn delay(self, delay: Duration) -> Self {
        self.delay_by(move |_| delay)
    }

    /// Each call sleeps for a time chosen from its arguments.
    pub fn delay_by(mut self, f: impl Fn(&Value) -> Duration + Send + Sync + 'static) -> Self {
        self.delay = Box::new(f);
        self
    }

    pub fn tool_calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn sql_calls(&self) -> Vec<SqlCall> {
        self.sql_calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    async fn busy(&self, args: &Value) -> Result<(), String> {
        TURN.try_with(|_| ())
            .map_err(|_| "called outside the turn's task".to_string())?;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep((self.delay)(args)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Host for Mock {
    fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name) && !self.hidden.iter().any(|h| h == name)
    }

    fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .keys()
            .filter(|name| self.has_tool(name))
            .cloned()
            .collect();
        names.sort();
        names
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push((tool.to_string(), args.clone()));
        }
        self.busy(&args).await?;
        match self.tools.get(tool) {
            Some(f) => f(&args),
            None => Err(format!("no tool {tool}")),
        }
    }

    async fn sql(
        &self,
        db: Value,
        query: &str,
        params: Vec<(String, Value)>,
    ) -> Result<Value, String> {
        if let Ok(mut calls) = self.sql_calls.lock() {
            calls.push(SqlCall {
                db: db.clone(),
                query: query.to_string(),
                params: params.clone(),
            });
        }
        self.busy(&db).await?;
        match &self.sql {
            Some(f) => f(&db, query, &params),
            None => Ok(json!([])),
        }
    }

    async fn read_artifact(&self, id: &str) -> Result<Value, String> {
        self.artifacts
            .get(id)
            .cloned()
            .ok_or_else(|| format!("artifact {id} not found or expired"))
    }

    async fn write_artifact(&self, name: &str, rows: Value) -> Result<Value, String> {
        let count = rows.as_array().map_or(0, Vec::len);
        self.written.lock().unwrap().push((name.to_string(), rows));
        Ok(json!({ "filename": name, "rows": count, "delivered": true }))
    }
}

pub(super) async fn exec_with(mock: &Arc<Mock>, source: &str, limits: Limits) -> Outcome {
    let host: Arc<dyn Host> = mock.clone();
    TURN.scope("turn-1", run(source, host, limits)).await
}

pub(super) async fn exec(mock: &Arc<Mock>, source: &str) -> Outcome {
    exec_with(mock, source, Limits::default()).await
}

pub(super) fn value(outcome: Outcome, source: &str) -> Value {
    match outcome {
        Outcome::Returned { value, .. } => value,
        other => panic!("{source}\n{}", other.render(source)),
    }
}

pub(super) fn failure(outcome: Outcome, source: &str) -> Diagnostic {
    match outcome {
        Outcome::Failed { error, .. } => error,
        other => panic!("expected a failure: {source}\n{}", other.render(source)),
    }
}

pub(super) fn rejection(outcome: Outcome, source: &str) -> Vec<Diagnostic> {
    match outcome {
        Outcome::Rejected { diagnostics } => diagnostics,
        other => panic!("expected a rejection: {source}\n{}", other.render(source)),
    }
}

/// Run a program that needs no host and return its value.
pub(super) async fn eval(source: &str) -> Value {
    value(exec(&Arc::new(Mock::new()), source).await, source)
}

/// Run a program that needs no host and return the error it fails with.
pub(super) async fn fail(source: &str) -> Diagnostic {
    failure(exec(&Arc::new(Mock::new()), source).await, source)
}

pub(super) fn at(d: &Diagnostic) -> (u32, u32) {
    (d.line, d.column)
}
