//! The tree-walking interpreter.
//!
//! Evaluation is async because tool calls and SQL blocks are. Outside
//! `all(...)` every call is awaited before the next expression runs. Inside
//! it, the elements of a list literal or the applications of a `.map`
//! callback are polled together on the calling task, results in order, and a
//! run-wide semaphore bounds the host calls in flight. Nothing is spawned,
//! so the host's task-locals stay in scope for every call.

use crate::ast::{
    Arm, BinaryOp, Body, Expr, ExprKind, Field, Lambda, Pattern, Segment, Stmt, UnaryOp,
};
use crate::diagnostic::{code, Diagnostic, Pos};
use crate::host::{Host, Limits};
use crate::library::{self, Builtin, Function, LibError, Method, FUNCTIONS, METHODS};
use crate::outcome::{JournalEntry, Stats};
use crate::suggest::closest;
use crate::value::{plural, Record, Value};
use futures::future::BoxFuture;
use futures::{StreamExt, TryStreamExt};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use tokio::sync::Semaphore;
use tokio::time::Instant;

type Eval<'a, T = Value> = BoxFuture<'a, Result<T, Diagnostic>>;

/// Steps between checks of the deadline during pure computation.
const CLOCK_EVERY: u64 = 1024;

/// How many suggestions an unknown name gets.
const SUGGESTIONS: usize = 5;

/// How much of the task's stack evaluation may use. The parser bounds
/// nesting in the source, but a lambda applied inside a lambda nests further.
/// A level costs about 2.5 KiB in a release build and 16 KiB in a debug one,
/// so the guard measures bytes rather than levels: half of a 2 MiB tokio
/// worker stack, leaving the rest to the host's frames above the run.
const MAX_STACK_BYTES: usize = 1 << 20;

/// The largest value a program may build, in [`Extent`] units: roughly one
/// per list element, record field or scalar, counting a shared part once per
/// reference. A SQL result of 50,000 rows by 19 columns fits.
///
/// [`Extent`]: crate::value::Extent
const MAX_VALUE_SIZE: u64 = 1_000_000;

/// How deeply values may nest; converting, printing, comparing and dropping
/// a value all recurse through it.
const MAX_VALUE_DEPTH: u32 = 200;

/// A lambda with the bindings it closed over.
pub(crate) struct Closure {
    lambda: Arc<Lambda>,
    env: Env,
}

/// Immutable bindings as a persistent list: binding a name shares the rest.
#[derive(Clone, Default)]
struct Env(Option<Arc<Frame>>);

struct Frame {
    name: String,
    value: Value,
    parent: Env,
}

impl Env {
    fn bind(&self, name: &str, value: Value) -> Env {
        Env(Some(Arc::new(Frame {
            name: name.to_string(),
            value,
            parent: self.clone(),
        })))
    }

    fn frames(&self) -> impl Iterator<Item = &Frame> {
        std::iter::successors(self.0.as_deref(), |frame| frame.parent.0.as_deref())
    }

    fn get(&self, name: &str) -> Option<&Value> {
        self.frames().find(|f| f.name == name).map(|f| &f.value)
    }
}

/// An evaluation that counts itself on the interpreter's depth while it is
/// polled, so the count is the nesting of the poll stack; the outermost one
/// in each poll marks where the stack started, and each nested one checks
/// how far below that it is. Futures polled together inside `all(...)` are
/// polled one after another, so siblings do not add up.
struct Nested<'a, T> {
    interp: &'a Interp,
    pos: Pos,
    inner: Eval<'a, T>,
}

impl<T> Future for Nested<'_, T> {
    type Output = Result<T, Diagnostic>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let here = stack_address();
        let interp = this.interp;
        if interp.depth.fetch_add(1, Ordering::Relaxed) == 0 {
            interp.stack_base.store(here, Ordering::Relaxed);
        }
        let used = here.abs_diff(interp.stack_base.load(Ordering::Relaxed));
        let polled = if used > MAX_STACK_BYTES {
            Poll::Ready(Err(error(
                code::NESTING,
                this.pos,
                format!(
                    "evaluation nests too deeply here ({} levels)",
                    interp.depth.load(Ordering::Relaxed)
                ),
            )
            .with_help(
                "lambdas calling lambdas nest; flatten them, or bind intermediate values with `let`",
            )))
        } else {
            this.inner.as_mut().poll(cx)
        };
        interp.depth.fetch_sub(1, Ordering::Relaxed);
        polled
    }
}

/// Where the stack is now: the address of a local.
#[inline(never)]
fn stack_address() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker) as *const u8 as usize
}

pub(crate) struct Interp {
    host: Arc<dyn Host>,
    limits: Limits,
    started: Instant,
    deadline: Option<Instant>,
    steps: AtomicU64,
    calls: AtomicU32,
    permits: Semaphore,
    journal: Mutex<Vec<JournalEntry>>,
    /// The position last evaluated, packed as `line << 32 | column`.
    last_pos: AtomicU64,
    /// Evaluations on the poll stack right now; see [`Nested`].
    depth: AtomicU32,
    /// The stack address of the outermost evaluation in the current poll.
    stack_base: AtomicUsize,
}

fn error(code: &'static str, pos: Pos, message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(code, pos, message)
}

fn lib_error(e: LibError, pos: Pos) -> Diagnostic {
    let diagnostic = error(code::ARGUMENT, pos, e.message);
    match e.help {
        Some(help) => diagnostic.with_help(help),
        None => diagnostic,
    }
}

fn pack(pos: Pos) -> u64 {
    (u64::from(pos.line) << 32) | u64::from(pos.column)
}

fn unpack(packed: u64) -> Pos {
    Pos {
        line: (packed >> 32) as u32,
        column: (packed & u64::from(u32::MAX)) as u32,
    }
}

fn did_you_mean(names: &[&str]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!("did you mean `{one}`?")),
        many => Some(format!(
            "did you mean one of: {}?",
            many.iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

impl Interp {
    pub(crate) fn new(host: Arc<dyn Host>, limits: Limits) -> Self {
        let started = Instant::now();
        let width = limits.max_concurrency.clamp(1, Semaphore::MAX_PERMITS);
        Self {
            host,
            deadline: started.checked_add(limits.deadline),
            limits,
            started,
            steps: AtomicU64::new(0),
            calls: AtomicU32::new(0),
            permits: Semaphore::new(width),
            journal: Mutex::new(Vec::new()),
            last_pos: AtomicU64::new(pack(Pos::START)),
            depth: AtomicU32::new(0),
            stack_base: AtomicUsize::new(0),
        }
    }

    pub(crate) fn stats(&self) -> Stats {
        Stats {
            steps: self.steps.load(Ordering::Relaxed),
            calls: self.calls.load(Ordering::Relaxed),
            elapsed: self.started.elapsed(),
            journal: self
                .journal
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }

    /// Run a checked program to its return value, as JSON within the size
    /// budget.
    pub(crate) async fn run(&self, program: &[Stmt]) -> Result<serde_json::Value, Diagnostic> {
        let flow = self
            .within(self.block(program, Env::default()))
            .await
            .map_err(|_| {
                self.deadline_passed("evaluating", unpack(self.last_pos.load(Ordering::Relaxed)))
            })??;
        let Some((value, pos)) = flow else {
            return Err(error(
                code::RETURN_MISSING,
                Pos::START,
                "the program ended without `return`",
            ));
        };
        let size = value.extent().size;
        if size > self.limits.max_return_bytes as u64 {
            return Err(self.return_too_large(format!("at least {size}"), pos));
        }
        let json = value.to_json().map_err(|not_data| {
            error(
                code::TYPE,
                pos,
                format!("the return value contains {}", not_data.describe()),
            )
            .with_help("return data: records, lists, strings, numbers, bools and null")
        })?;
        let size = json.to_string().len();
        if size > self.limits.max_return_bytes {
            return Err(self.return_too_large(size.to_string(), pos));
        }
        Ok(json)
    }

    fn return_too_large(&self, size: String, pos: Pos) -> Diagnostic {
        error(
            code::BUDGET_RETURN,
            pos,
            format!(
                "the return value is {size} bytes; the limit is {}",
                self.limits.max_return_bytes
            ),
        )
        .with_help("return an aggregate (`count`, `sum`, `group_by`) or a sample with `.take(n)`")
    }

    /// Refuse a value too large or deep to walk. Every expression's value
    /// passes here, so nothing larger is ever built on.
    fn admit(&self, value: Value, pos: Pos) -> Result<Value, Diagnostic> {
        let extent = value.extent();
        if extent.size > MAX_VALUE_SIZE {
            return Err(error(
                code::VALUE_SIZE,
                pos,
                format!(
                    "this value has {} parts, counting each repeat of a shared part; the limit is {MAX_VALUE_SIZE}",
                    extent.size
                ),
            )
            .with_help("aggregate (`count`, `sum`, `group_by`) or sample with `.take(n)` instead of repeating values"));
        }
        if extent.depth > MAX_VALUE_DEPTH {
            return Err(error(
                code::NESTING,
                pos,
                format!(
                    "this value nests {} levels deep; the limit is {MAX_VALUE_DEPTH}",
                    extent.depth
                ),
            ));
        }
        Ok(value)
    }

    async fn within<F: Future>(&self, future: F) -> Result<F::Output, tokio::time::error::Elapsed> {
        match self.deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, future).await,
            None => Ok(future.await),
        }
    }

    fn deadline_passed(&self, during: &str, pos: Pos) -> Diagnostic {
        error(
            code::BUDGET_TIME,
            pos,
            format!(
                "the run's deadline of {:?} passed while {during}",
                self.limits.deadline
            ),
        )
        .with_help(
            "make fewer or narrower calls, or run independent calls together with `all(...)`",
        )
    }

    fn step(&self, pos: Pos) -> Result<(), Diagnostic> {
        self.charge(1, pos)
    }

    /// Spend `n` steps at `pos`; every so often, also check the clock.
    fn charge(&self, n: u64, pos: Pos) -> Result<(), Diagnostic> {
        self.last_pos.store(pack(pos), Ordering::Relaxed);
        let before = self.steps.fetch_add(n, Ordering::Relaxed);
        let after = before.saturating_add(n);
        if after > self.limits.max_steps {
            return Err(error(
                code::BUDGET_STEPS,
                pos,
                format!("the run used all of its {} steps", self.limits.max_steps),
            )
            .with_help("filter, join and aggregate in SQL, or work on fewer rows"));
        }
        let crossed = before / CLOCK_EVERY != after / CLOCK_EVERY;
        if crossed && self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(self.deadline_passed("evaluating", pos));
        }
        Ok(())
    }

    /// Run statements; `Some` once a `return` runs.
    fn block<'a>(&'a self, stmts: &'a [Stmt], mut env: Env) -> Nested<'a, Option<(Value, Pos)>> {
        let pos = stmts.first().map_or(Pos::START, Stmt::pos);
        self.nested(pos, async move {
            for stmt in stmts {
                match stmt {
                    Stmt::Let { name, value, pos } => {
                        self.step(*pos)?;
                        let value = self.eval(value, env.clone()).await?;
                        env = env.bind(name, value);
                    }
                    Stmt::If {
                        branches,
                        otherwise,
                    } => {
                        if let Some((condition, _)) = branches.first() {
                            self.step(condition.pos)?;
                        }
                        let mut chosen = otherwise.as_deref();
                        for (condition, body) in branches {
                            if self.condition(condition, &env, "`if`").await? {
                                chosen = Some(body);
                                break;
                            }
                        }
                        if let Some(body) = chosen {
                            if let Some(returned) = self.block(body, env.clone()).await? {
                                return Ok(Some(returned));
                            }
                        }
                    }
                    Stmt::For {
                        var,
                        iterable,
                        body,
                        pos,
                    } => {
                        self.step(*pos)?;
                        let items = match self.eval(iterable, env.clone()).await? {
                            Value::List(items) => items,
                            other => {
                                return Err(error(
                                    code::TYPE,
                                    iterable.pos,
                                    format!("`for` needs a list; got {}", other.describe()),
                                ))
                            }
                        };
                        for item in items.iter() {
                            if let Some(returned) =
                                self.block(body, env.bind(var, item.clone())).await?
                            {
                                return Ok(Some(returned));
                            }
                        }
                    }
                    Stmt::Return { value, pos } => {
                        self.step(*pos)?;
                        return Ok(Some((self.eval(value, env.clone()).await?, *pos)));
                    }
                }
            }
            Ok(None)
        })
    }

    fn nested<'a, T>(
        &'a self,
        pos: Pos,
        future: impl Future<Output = Result<T, Diagnostic>> + Send + 'a,
    ) -> Nested<'a, T> {
        Nested {
            interp: self,
            pos,
            inner: Box::pin(future),
        }
    }

    async fn condition(&self, expr: &Expr, env: &Env, what: &str) -> Result<bool, Diagnostic> {
        match self.eval(expr, env.clone()).await? {
            Value::Bool(b) => Ok(b),
            other => Err(error(
                code::TYPE,
                expr.pos,
                format!("{what} needs a bool; got {}", other.describe()),
            )
            .with_help("compare explicitly, e.g. `xs.length > 0` or `x != null`")),
        }
    }

    fn eval<'a>(&'a self, expr: &'a Expr, env: Env) -> Nested<'a, Value> {
        self.nested(expr.pos, async move {
            let pos = expr.pos;
            self.step(pos)?;
            let value = match &expr.kind {
                ExprKind::Null => Value::Null,
                ExprKind::Bool(b) => Value::Bool(*b),
                ExprKind::Int(n) => Value::Int(*n),
                ExprKind::Float(n) => Value::Float(*n),
                ExprKind::Str(s) => Value::str(s.as_str()),
                ExprKind::Template(segments) => boxed(|| self.template(segments, env, pos)).await?,
                ExprKind::List(items) => Value::list(boxed(|| self.eval_each(items, &env)).await?),
                ExprKind::Record(fields) => boxed(|| self.record(fields, env)).await?,
                ExprKind::Name(name) => self.name(name, &env, pos)?,
                ExprKind::Member { .. } | ExprKind::Index { .. } | ExprKind::Method { .. } => {
                    self.chain(expr, env, false).await?.unwrap_or(Value::Null)
                }
                ExprKind::Call { name, args } => boxed(|| self.call(name, args, env, pos)).await?,
                ExprKind::Lambda(lambda) => Value::Function(Arc::new(Closure {
                    lambda: lambda.clone(),
                    env,
                })),
                ExprKind::Unary { op, operand } => unary(*op, self.eval(operand, env).await?, pos)?,
                ExprKind::Binary { op, left, right } => {
                    boxed(|| self.binary(*op, left, right, env, pos)).await?
                }
                ExprKind::Match {
                    scrutinee,
                    arms,
                    default,
                } => boxed(|| self.match_arm(scrutinee, arms, default, env)).await?,
                ExprKind::Cond {
                    cond,
                    then,
                    otherwise,
                } => {
                    let chosen = match self.condition(cond, &env, "`cond ? a : b`").await? {
                        true => then,
                        false => otherwise,
                    };
                    self.eval(chosen, env).await?
                }
                ExprKind::Sql { db, parts } => boxed(|| self.sql(db, parts, env, pos)).await?,
            };
            self.admit(value, pos)
        })
    }

    async fn template(
        &self,
        segments: &[Segment],
        env: Env,
        pos: Pos,
    ) -> Result<Value, Diagnostic> {
        let mut text = String::new();
        for segment in segments {
            match segment {
                Segment::Text(t) => text.push_str(t),
                Segment::Hole(hole) => {
                    let value = self.eval(hole, env.clone()).await?;
                    self.charge(value.extent().size, hole.pos)?;
                    text.push_str(&value.display())
                }
            }
        }
        let text = Value::str(text);
        self.charge(text.weight(), pos)?;
        Ok(text)
    }

    async fn record(&self, fields: &[Field], env: Env) -> Result<Value, Diagnostic> {
        let mut values: Vec<(String, Value)> = Vec::with_capacity(fields.len());
        let mut set = |key: &str, value: Value| match values.iter_mut().find(|(k, _)| k == key) {
            Some(existing) => existing.1 = value,
            None => values.push((key.to_string(), value)),
        };
        for field in fields {
            match field {
                Field::Pair(key, value) => set(key, self.eval(value, env.clone()).await?),
                Field::Spread(spread) => match self.eval(spread, env.clone()).await? {
                    Value::Record(record) => {
                        for (key, value) in record.fields() {
                            set(key, value.clone());
                        }
                    }
                    Value::Null => {}
                    other => {
                        return Err(error(
                            code::TYPE,
                            spread.pos,
                            format!("`...` needs a record; got {}", other.describe()),
                        )
                        .with_help("concatenate lists with `+`"))
                    }
                },
            }
        }
        Ok(Value::record(values))
    }

    async fn match_arm(
        &self,
        scrutinee: &Expr,
        arms: &[Arm],
        default: &Expr,
        env: Env,
    ) -> Result<Value, Diagnostic> {
        let value = self.eval(scrutinee, env.clone()).await?;
        if !matches!(value, Value::Str(_) | Value::Int(_) | Value::Bool(_)) {
            return Err(error(
                code::TYPE,
                scrutinee.pos,
                format!(
                    "`match` needs a string, int or bool; got {}",
                    value.describe()
                ),
            ));
        }
        let arm = arms
            .iter()
            .find(|arm| arm.patterns.iter().any(|p| matches_pattern(p, &value)));
        self.eval(arm.map_or(default, |arm| &arm.value), env).await
    }

    /// Evaluate a member, index or method link. `None` when a `?.` met null,
    /// which makes the whole chain null. `concurrent` runs the outermost
    /// `.map` as `all(...)` does.
    fn chain<'a>(&'a self, expr: &'a Expr, env: Env, concurrent: bool) -> Eval<'a, Option<Value>> {
        Box::pin(async move {
            let pos = expr.pos;
            match &expr.kind {
                ExprKind::Member {
                    object,
                    field,
                    optional,
                } => {
                    let Some(object) = self.link(object, env).await? else {
                        return Ok(None);
                    };
                    if *optional && matches!(object, Value::Null) {
                        return Ok(None);
                    }
                    member(&object, field, *optional, pos).map(Some)
                }
                ExprKind::Index { object, index } => {
                    let Some(object) = self.link(object, env.clone()).await? else {
                        return Ok(None);
                    };
                    index_into(&object, &self.eval(index, env).await?, pos).map(Some)
                }
                ExprKind::Method {
                    receiver,
                    name,
                    args,
                    optional,
                } => {
                    let Some(receiver) = self.link(receiver, env.clone()).await? else {
                        return Ok(None);
                    };
                    if *optional && matches!(receiver, Value::Null) {
                        return Ok(None);
                    }
                    self.method(receiver, name, args, env, pos, concurrent)
                        .await
                        .map(Some)
                }
                _ => self.eval(expr, env).await.map(Some),
            }
        })
    }

    /// The object of a chain link: another link, or any expression.
    async fn link(&self, object: &Expr, env: Env) -> Result<Option<Value>, Diagnostic> {
        match object.kind {
            ExprKind::Member { .. } | ExprKind::Index { .. } | ExprKind::Method { .. } => {
                self.step(object.pos)?;
                self.chain(object, env, false).await
            }
            _ => self.eval(object, env).await.map(Some),
        }
    }

    async fn eval_each(&self, exprs: &[Expr], env: &Env) -> Result<Vec<Value>, Diagnostic> {
        let mut values = Vec::with_capacity(exprs.len());
        for expr in exprs {
            values.push(self.eval(expr, env.clone()).await?);
        }
        Ok(values)
    }

    fn name(&self, name: &str, env: &Env, pos: Pos) -> Result<Value, Diagnostic> {
        if let Some(value) = env.get(name) {
            return Ok(value.clone());
        }
        if library::function(name).is_some() || self.host.has_tool(name) {
            return Err(error(
                code::TYPE,
                pos,
                format!("`{name}` is a function and must be called"),
            )
            .with_fix(format!("{name}(...)")));
        }
        let locals: Vec<&str> = env.frames().map(|f| f.name.as_str()).collect();
        let diagnostic = error(code::UNKNOWN_NAME, pos, format!("unknown name `{name}`"));
        Err(match did_you_mean(&closest(name, locals, SUGGESTIONS)) {
            Some(help) => diagnostic.with_help(help),
            None => diagnostic,
        })
    }

    async fn call(
        &self,
        name: &str,
        args: &[Expr],
        env: Env,
        pos: Pos,
    ) -> Result<Value, Diagnostic> {
        if let Some(callee) = env.get(name).cloned() {
            let values = self.eval_each(args, &env).await?;
            return match callee {
                Value::Function(_) => {
                    let context = format!("`{name}`");
                    boxed(|| self.apply(&callee, values, pos, &context)).await
                }
                other => Err(error(
                    code::TYPE,
                    pos,
                    format!("`{name}` is {}, not a function", other.describe()),
                )),
            };
        }
        if let Some(function) = library::function(name) {
            if args.len() != function.builtin.arity() {
                return Err(arity(function, args.len(), pos));
            }
            if let (Builtin::All, [arg]) = (function.builtin, args) {
                return boxed(|| self.all(arg, env, pos)).await;
            }
            let values = self.eval_each(args, &env).await?;
            if matches!(
                function.builtin,
                Builtin::ArtifactRead | Builtin::ArtifactWrite
            ) {
                return boxed(|| self.artifact(function.builtin, values, pos)).await;
            }
            return boxed(|| self.builtin(function, values, pos)).await;
        }
        if self.host.has_tool(name) {
            let values = self.eval_each(args, &env).await?;
            return boxed(|| self.tool(name, values, pos)).await;
        }
        let tools = self.host.tool_names();
        let known = FUNCTIONS
            .iter()
            .map(|f| f.name)
            .chain(tools.iter().map(String::as_str));
        let diagnostic = error(
            code::UNKNOWN_NAME,
            pos,
            format!("unknown function `{name}`"),
        );
        Err(match did_you_mean(&closest(name, known, SUGGESTIONS)) {
            Some(help) => diagnostic.with_help(help),
            None => diagnostic.with_help("no library function or callable tool has that name"),
        })
    }

    /// Apply a function value to arguments; `context` names the caller in
    /// errors.
    async fn apply(
        &self,
        callee: &Value,
        args: Vec<Value>,
        pos: Pos,
        context: &str,
    ) -> Result<Value, Diagnostic> {
        let Value::Function(closure) = callee else {
            return Err(error(
                code::ARGUMENT,
                pos,
                format!(
                    "{context} needs a function like `x => ...`; got {}",
                    callee.describe()
                ),
            ));
        };
        let params = &closure.lambda.params;
        if params.len() != args.len() {
            return Err(error(
                code::ARITY,
                pos,
                format!(
                    "{context} calls its function with {} argument{}, but the function takes {}",
                    args.len(),
                    plural(args.len()),
                    params.len()
                ),
            ));
        }
        let env = params
            .iter()
            .zip(args)
            .fold(closure.env.clone(), |env, (param, arg)| {
                env.bind(param, arg)
            });
        match &closure.lambda.body {
            Body::Expr(body) => self.eval(body, env).await,
            Body::Block(body) => match self.block(body, env).await? {
                Some((value, _)) => Ok(value),
                None => Err(error(
                    code::RETURN_MISSING,
                    pos,
                    format!("{context} called a function that ended without `return`"),
                )
                .with_help("give every branch of the function body a `return`")),
            },
        }
    }

    /// Apply `f` to each item in turn.
    async fn apply_each(
        &self,
        f: &Value,
        items: &[Value],
        pos: Pos,
        context: &str,
    ) -> Result<Vec<Value>, Diagnostic> {
        let mut results = Vec::with_capacity(items.len());
        for item in items {
            results.push(self.apply(f, vec![item.clone()], pos, context).await?);
        }
        Ok(results)
    }

    async fn predicate(
        &self,
        f: &Value,
        item: &Value,
        index: usize,
        pos: Pos,
        context: &str,
    ) -> Result<bool, Diagnostic> {
        match self.apply(f, vec![item.clone()], pos, context).await? {
            Value::Bool(b) => Ok(b),
            other => Err(error(
                code::TYPE,
                pos,
                format!(
                    "{context}'s function must return a bool; for element {index} it returned {}",
                    other.describe()
                ),
            )),
        }
    }

    /// Poll `futures` together, at most the concurrency limit at a time,
    /// collecting results in order. Callers map over indexes rather than
    /// `&item`: a closure taking a borrowed argument makes the future's
    /// `Send` bound higher-ranked, which rustc cannot prove.
    async fn concurrently<F>(
        &self,
        futures: impl Iterator<Item = F>,
    ) -> Result<Vec<Value>, Diagnostic>
    where
        F: Future<Output = Result<Value, Diagnostic>>,
    {
        let width = self.limits.max_concurrency.max(1);
        futures::stream::iter(futures)
            .buffered(width)
            .try_collect()
            .await
    }

    async fn all(&self, arg: &Expr, env: Env, pos: Pos) -> Result<Value, Diagnostic> {
        match &arg.kind {
            ExprKind::List(items) => {
                self.step(arg.pos)?;
                let values = self
                    .concurrently((0..items.len()).map(|i| self.eval(&items[i], env.clone())))
                    .await?;
                Ok(Value::list(values))
            }
            ExprKind::Method { name, .. } if name == "map" => {
                self.step(arg.pos)?;
                Ok(self.chain(arg, env, true).await?.unwrap_or(Value::Null))
            }
            _ => match self.eval(arg, env).await? {
                list @ Value::List(_) => Ok(list),
                other => Err(error(
                    code::ARGUMENT,
                    pos,
                    format!("`all` needs a list; got {}", other.describe()),
                )
                .with_help(
                    "write `all([call_a(...), call_b(...)])` or `all(xs.map(x => call(...)))`",
                )),
            },
        }
    }

    async fn builtin(
        &self,
        function: &Function,
        args: Vec<Value>,
        pos: Pos,
    ) -> Result<Value, Diagnostic> {
        let cost = |arg: &Value| {
            if function.walks {
                arg.extent().size
            } else {
                arg.weight()
            }
        };
        self.charge(args.iter().map(cost).fold(0, u64::saturating_add), pos)?;
        let name = function.name;
        let context = format!("`{name}`");
        let checked = |result: Result<Value, LibError>| result.map_err(|e| lib_error(e, pos));
        let items = |value| library::list(name, value).map_err(|e| lib_error(e, pos));
        match (function.builtin, args.as_slice()) {
            (Builtin::Unary(f), [a]) => checked(f(a)),
            (Builtin::Binary(f), [a, b]) => checked(f(a, b)),
            (Builtin::GroupBy, [xs, key]) => {
                let xs = items(xs)?;
                let keys = self.apply_each(key, xs, pos, &context).await?;
                checked(library::group(xs, keys))
            }
            (Builtin::SortBy { descending }, [xs, key]) => {
                let xs = items(xs)?;
                let keys = self.apply_each(key, xs, pos, &context).await?;
                checked(library::sort(name, xs, keys, descending))
            }
            (Builtin::Join, [left, right, left_key, right_key]) => {
                let (left, right) = (items(left)?, items(right)?);
                let left_keys = self.apply_each(left_key, left, pos, &context).await?;
                let right_keys = self.apply_each(right_key, right, pos, &context).await?;
                let index = library::JoinIndex::new(&left_keys, &right_keys)
                    .map_err(|e| lib_error(e, pos))?;
                self.charge(index.pairs(), pos)?;
                Ok(index.build(left, right))
            }
            _ => Err(arity(function, args.len(), pos)),
        }
    }

    async fn method(
        &self,
        receiver: Value,
        name: &str,
        args: &[Expr],
        env: Env,
        pos: Pos,
        concurrent: bool,
    ) -> Result<Value, Diagnostic> {
        let Some((method, signature)) = library::method(name) else {
            return Err(unknown_method(name, &receiver, pos));
        };
        if matches!(receiver, Value::Null) {
            return Err(
                error(code::NULL, pos, format!("cannot call `.{name}` on null"))
                    .with_fix(format!("?.{name}(...)")),
            );
        }
        let items = library::method_receiver(name, &receiver).map_err(|e| lib_error(e, pos))?;
        let [arg] = args else {
            return Err(error(
                code::ARITY,
                pos,
                format!("`.{name}` takes 1 argument; got {}", args.len()),
            )
            .with_help(format!("usage: {signature}")));
        };
        let arg = self.eval(arg, env).await?;
        self.charge(items.len() as u64, pos)?;
        let context = format!("`.{name}`");
        let context = context.as_str();
        match method {
            Method::Take => library::take(items, &arg).map_err(|e| lib_error(e, pos)),
            Method::Map if concurrent => Ok(Value::list(
                self.concurrently(
                    (0..items.len())
                        .map(|i| self.apply(&arg, vec![items[i].clone()], pos, context)),
                )
                .await?,
            )),
            Method::Map => Ok(Value::list(
                self.apply_each(&arg, items, pos, context).await?,
            )),
            Method::Filter => {
                let mut kept = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    if self.predicate(&arg, item, index, pos, context).await? {
                        kept.push(item.clone());
                    }
                }
                Ok(Value::list(kept))
            }
            Method::FlatMap => {
                let mut flat = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    match self.apply(&arg, vec![item.clone()], pos, context).await? {
                        Value::List(inner) => {
                            self.charge(inner.len() as u64, pos)?;
                            flat.extend(inner.iter().cloned());
                        }
                        other => {
                            return Err(error(
                                code::TYPE,
                                pos,
                                format!(
                                    "`.flat_map`'s function must return a list; for element {index} it returned {}",
                                    other.describe()
                                ),
                            ))
                        }
                    }
                }
                Ok(Value::list(flat))
            }
            Method::Find => {
                for (index, item) in items.iter().enumerate() {
                    if self.predicate(&arg, item, index, pos, context).await? {
                        return Ok(item.clone());
                    }
                }
                Ok(Value::Null)
            }
            Method::Any | Method::Every => {
                let wanted = method == Method::Any;
                for (index, item) in items.iter().enumerate() {
                    if self.predicate(&arg, item, index, pos, context).await? == wanted {
                        return Ok(Value::Bool(wanted));
                    }
                }
                Ok(Value::Bool(!wanted))
            }
        }
    }

    async fn binary(
        &self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        env: Env,
        pos: Pos,
    ) -> Result<Value, Diagnostic> {
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let what = format!("`{}`", op.symbol());
                let left = self
                    .condition(left, &env, &format!("the left side of {what}"))
                    .await?;
                if left == (op == BinaryOp::Or) {
                    return Ok(Value::Bool(left));
                }
                let right = self
                    .condition(right, &env, &format!("the right side of {what}"))
                    .await?;
                Ok(Value::Bool(right))
            }
            BinaryOp::Coalesce => match self.eval(left, env.clone()).await? {
                Value::Null => self.eval(right, env).await,
                value => Ok(value),
            },
            _ => {
                let left = self.eval(left, env.clone()).await?;
                let right = self.eval(right, env).await?;
                let weight = match (op, &left, &right) {
                    (BinaryOp::Eq | BinaryOp::Ne, _, _) => {
                        left.extent().size.min(right.extent().size)
                    }
                    (_, Value::List(_), Value::List(_)) | (_, Value::Str(_), Value::Str(_)) => {
                        left.weight() + right.weight()
                    }
                    _ => 0,
                };
                self.charge(weight, pos)?;
                arithmetic(op, &left, &right, pos)
            }
        }
    }

    async fn tool(&self, name: &str, args: Vec<Value>, pos: Pos) -> Result<Value, Diagnostic> {
        let [arg @ Value::Record(_)] = args.as_slice() else {
            let got = match args.as_slice() {
                [only] => only.describe(),
                _ => format!("{} arguments", args.len()),
            };
            return Err(error(
                code::ARGUMENT,
                pos,
                format!("`{name}` takes one record of arguments; got {got}"),
            )
            .with_fix(format!("{name}({{ ... }})")));
        };
        self.charge(arg.extent().size, pos)?;
        let json = arg.to_json().map_err(|not_data| {
            error(
                code::ARGUMENT,
                pos,
                format!("`{name}`'s arguments contain {}", not_data.describe()),
            )
        })?;
        match self
            .host_call(name, pos, self.host.call(name, json))
            .await?
        {
            Ok(result) => Ok(Value::from_json(result)),
            Err(message) => Err(error(
                code::TOOL,
                pos,
                format!("`{name}` failed: {message}"),
            )),
        }
    }

    /// `artifact_read(id)` and `artifact_write(name, rows)`: host calls,
    /// counted and timed like tool calls.
    async fn artifact(
        &self,
        which: Builtin,
        args: Vec<Value>,
        pos: Pos,
    ) -> Result<Value, Diagnostic> {
        let text = |value: &Value, what: &str| match value {
            Value::Str(s) => Ok(s.to_string()),
            other => Err(error(
                code::ARGUMENT,
                pos,
                format!("{what} must be a string; got {}", other.describe()),
            )),
        };
        let (label, outcome) = match (which, args.as_slice()) {
            (Builtin::ArtifactRead, [id]) => {
                let id = text(id, "`artifact_read`'s artifact id")?;
                (
                    "artifact_read",
                    self.host_call("artifact_read", pos, self.host.read_artifact(&id))
                        .await?,
                )
            }
            (Builtin::ArtifactWrite, [name, rows]) => {
                let name = text(name, "`artifact_write`'s filename")?;
                if !matches!(rows, Value::List(_)) {
                    return Err(error(
                        code::ARGUMENT,
                        pos,
                        format!(
                            "`artifact_write` needs a list of rows; got {}",
                            rows.describe()
                        ),
                    ));
                }
                self.charge(rows.extent().size, pos)?;
                let json = rows.to_json().map_err(|not_data| {
                    error(
                        code::ARGUMENT,
                        pos,
                        format!("`artifact_write`'s rows contain {}", not_data.describe()),
                    )
                })?;
                (
                    "artifact_write",
                    self.host_call("artifact_write", pos, self.host.write_artifact(&name, json))
                        .await?,
                )
            }
            _ => unreachable!("the caller passes only artifact builtins at their arity"),
        };
        match outcome {
            Ok(result) => self.admit(Value::from_json(result), pos),
            Err(message) => Err(error(
                code::TOOL,
                pos,
                format!("`{label}` failed: {message}"),
            )),
        }
    }

    async fn sql(
        &self,
        db: &Expr,
        parts: &[Segment],
        env: Env,
        pos: Pos,
    ) -> Result<Value, Diagnostic> {
        let data = |value: Value, at: Pos, what: &str| {
            self.charge(value.extent().size, at)?;
            value.to_json().map_err(|not_data| {
                error(
                    code::ARGUMENT,
                    at,
                    format!("{what} of `sql` is {}, not data", not_data.describe()),
                )
            })
        };
        let db_value = self.eval(db, env.clone()).await?;
        let db_json = data(db_value, db.pos, "the database")?;
        let mut query = String::new();
        let mut params = Vec::new();
        for part in parts {
            match part {
                Segment::Text(text) => query.push_str(text),
                Segment::Hole(hole) => {
                    let value = self.eval(hole, env.clone()).await?;
                    let name = format!("p{}", params.len() + 1);
                    query.push_str(&format!("{{{{{name}}}}}"));
                    params.push((name, data(value, hole.pos, "a parameter")?));
                }
            }
        }
        match self
            .host_call("sql", pos, self.host.sql(db_json, &query, params))
            .await?
        {
            Ok(rows) => Ok(Value::from_json(rows)),
            Err(message) => Err(error(
                code::SQL,
                pos,
                format!("the SQL query failed: {message}"),
            )),
        }
    }

    /// Run one host call within the call budget, the concurrency limit and
    /// the deadline, and journal it.
    async fn host_call<F>(
        &self,
        label: &str,
        pos: Pos,
        call: F,
    ) -> Result<Result<serde_json::Value, String>, Diagnostic>
    where
        F: Future<Output = Result<serde_json::Value, String>>,
    {
        let max = self.limits.max_calls;
        self.calls
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < max).then_some(n + 1)
            })
            .map_err(|made| {
                error(
                    code::BUDGET_CALLS,
                    pos,
                    format!(
                        "the run may make {max} tool and SQL calls; `{label}` would be call {}",
                        made.saturating_add(1)
                    ),
                )
                .with_help("fetch in bulk: one SQL query or search instead of a call per item")
            })?;
        let started = Instant::now();
        let outcome = self
            .within(async {
                match self.permits.acquire().await {
                    Ok(_permit) => call.await,
                    Err(closed) => Err(closed.to_string()),
                }
            })
            .await;
        self.journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(JournalEntry {
                tool: label.to_string(),
                ok: matches!(outcome, Ok(Ok(_))),
                duration: started.elapsed(),
            });
        outcome.map_err(|_| self.deadline_passed(&format!("`{label}` was running"), pos))
    }
}

/// Build a future in this short-lived frame and box it. Awaiting a future
/// built inline keeps a slot of its full size in the awaiting poll frame,
/// which in a debug build is never reused; boxed, the slot is a pointer, so
/// each level of nesting costs less stack.
#[inline(never)]
fn boxed<'a, T, F>(make: impl FnOnce() -> F) -> Eval<'a, T>
where
    F: Future<Output = Result<T, Diagnostic>> + Send + 'a,
{
    Box::pin(make())
}

fn arity(function: &Function, got: usize, pos: Pos) -> Diagnostic {
    let wanted = function.builtin.arity();
    error(
        code::ARITY,
        pos,
        format!(
            "`{}` takes {wanted} argument{}; got {got}",
            function.name,
            plural(wanted)
        ),
    )
    .with_help(format!("usage: {}", function.signature))
}

fn unknown_method(name: &str, receiver: &Value, pos: Pos) -> Diagnostic {
    let diagnostic = error(
        code::UNKNOWN_NAME,
        pos,
        format!("{} has no method `.{name}`", receiver.type_name()),
    );
    if name == "length" {
        return diagnostic
            .with_help("`.length` is a property")
            .with_fix(".length");
    }
    if let Some(function) = library::function(name) {
        return diagnostic
            .with_help(format!("`{name}` is a function, not a method"))
            .with_fix(function.signature);
    }
    let names = METHODS.iter().map(|(n, _, _)| *n).chain(["length"]);
    match did_you_mean(&closest(name, names, SUGGESTIONS)) {
        Some(help) => diagnostic.with_help(help),
        None => diagnostic,
    }
}

fn missing_field(record: &Record, field: &str, pos: Pos) -> Diagnostic {
    const SHOWN: usize = 20;
    let fields: Vec<&str> = record.keys().collect();
    let listed = if fields.is_empty() {
        "it has no fields".to_string()
    } else {
        let mut listed = format!(
            "its fields are: {}",
            fields[..fields.len().min(SHOWN)].join(", ")
        );
        if fields.len() > SHOWN {
            listed.push_str(&format!(" and {} more", fields.len() - SHOWN));
        }
        listed
    };
    let diagnostic = error(
        code::FIELD,
        pos,
        format!("the record has no field `{field}`; {listed}"),
    )
    .with_help(format!("use `?.{field}` if it may be absent"));
    match closest(field, fields, 1).first() {
        Some(best) => diagnostic.with_fix(format!(".{best}")),
        None => diagnostic,
    }
}

fn member(value: &Value, field: &str, optional: bool, pos: Pos) -> Result<Value, Diagnostic> {
    match value {
        Value::Record(record) => match record.get(field) {
            Some(value) => Ok(value.clone()),
            None if optional => Ok(Value::Null),
            None => Err(missing_field(record, field, pos)),
        },
        Value::Null => Err(
            error(code::NULL, pos, format!("cannot read `.{field}` of null"))
                .with_help("`?.` gives null instead, and `?? default` replaces a null")
                .with_fix(format!("?.{field}")),
        ),
        Value::List(items) if field == "length" => Ok(Value::Int(items.len() as i64)),
        Value::Str(s) if field == "length" => Ok(Value::Int(s.chars().count() as i64)),
        Value::List(_) => Err(error(
            code::TYPE,
            pos,
            format!("a list has no field `{field}`; its only property is `.length`"),
        )
        .with_fix(format!("xs.map(x => x.{field})"))),
        other => Err(error(
            code::TYPE,
            pos,
            format!("cannot read `.{field}` of {}", other.describe()),
        )),
    }
}

fn index_into(object: &Value, index: &Value, pos: Pos) -> Result<Value, Diagnostic> {
    match (object, index) {
        (Value::List(items), Value::Int(i)) => usize::try_from(*i)
            .ok()
            .and_then(|i| items.get(i))
            .cloned()
            .ok_or_else(|| {
                error(
                    code::INDEX,
                    pos,
                    format!(
                        "index {i} is out of range for a list of {} element{}",
                        items.len(),
                        plural(items.len())
                    ),
                )
                .with_help("`first(xs)` gives null for an empty list")
            }),
        (Value::Record(record), Value::Str(key)) => record
            .get(key)
            .cloned()
            .ok_or_else(|| missing_field(record, key, pos)),
        (Value::List(_), other) => Err(error(
            code::TYPE,
            pos,
            format!("a list index must be an int; got {}", other.describe()),
        )),
        (Value::Record(_), other) => Err(error(
            code::TYPE,
            pos,
            format!("a record key must be a string; got {}", other.describe()),
        )),
        (Value::Null, _) => {
            Err(error(code::NULL, pos, "cannot index null")
                .with_help("`?? default` replaces a null"))
        }
        (other, _) => Err(error(
            code::TYPE,
            pos,
            format!(
                "cannot index {}; only lists and records can be indexed",
                other.describe()
            ),
        )),
    }
}

fn matches_pattern(pattern: &Pattern, value: &Value) -> bool {
    match (pattern, value) {
        (Pattern::Str(p), Value::Str(s)) => p.as_str() == s.as_ref(),
        (Pattern::Int(p), Value::Int(n)) => p == n,
        (Pattern::Bool(p), Value::Bool(b)) => p == b,
        _ => false,
    }
}

fn unary(op: UnaryOp, value: Value, pos: Pos) -> Result<Value, Diagnostic> {
    match (op, &value) {
        (UnaryOp::Neg, Value::Int(n)) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| error(code::ARITHMETIC, pos, "`-` overflowed a 64-bit int")),
        (UnaryOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
        (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (UnaryOp::Neg, other) => Err(error(
            code::TYPE,
            pos,
            format!("`-` needs a number; got {}", other.describe()),
        )),
        (UnaryOp::Not, other) => Err(error(
            code::TYPE,
            pos,
            format!("`!` needs a bool; got {}", other.describe()),
        )
        .with_help("compare explicitly, e.g. `x == null` or `xs.length == 0`")),
    }
}

fn arithmetic(op: BinaryOp, left: &Value, right: &Value, pos: Pos) -> Result<Value, Diagnostic> {
    let symbol = op.symbol();
    let mismatch = |needs: &str| {
        let diagnostic = error(
            code::TYPE,
            pos,
            format!(
                "`{symbol}` needs {needs}; got {} and {}",
                left.describe(),
                right.describe()
            ),
        );
        if matches!(left, Value::Str(_)) != matches!(right, Value::Str(_)) {
            diagnostic.with_help(
                "there are no implicit conversions: use `str(x)`, `int(s)` or `float(s)`",
            )
        } else {
            diagnostic
        }
    };
    match op {
        BinaryOp::Eq => return Ok(Value::Bool(left.equals(right))),
        BinaryOp::Ne => return Ok(Value::Bool(!left.equals(right))),
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            let ordering = left
                .compare(right)
                .ok_or_else(|| mismatch("two numbers or two strings"))?;
            return Ok(Value::Bool(match op {
                BinaryOp::Lt => ordering.is_lt(),
                BinaryOp::Le => ordering.is_le(),
                BinaryOp::Gt => ordering.is_gt(),
                _ => ordering.is_ge(),
            }));
        }
        _ => {}
    }
    match (op, left, right) {
        (BinaryOp::Add, Value::Str(a), Value::Str(b)) => return Ok(Value::str(format!("{a}{b}"))),
        (BinaryOp::Add, Value::List(a), Value::List(b)) => {
            return Ok(Value::list(a.iter().chain(b.iter()).cloned().collect()))
        }
        _ => {}
    }
    let zero_divisor =
        matches!(op, BinaryOp::Div | BinaryOp::Rem) && right.as_number().is_some_and(|n| n == 0.0);
    if zero_divisor && left.as_number().is_some() {
        return Err(error(code::ARITHMETIC, pos, format!("`{symbol}` by zero")));
    }
    match (left, right) {
        (Value::Int(a), Value::Int(b)) if op != BinaryOp::Div => {
            let result = match op {
                BinaryOp::Add => a.checked_add(*b),
                BinaryOp::Sub => a.checked_sub(*b),
                BinaryOp::Mul => a.checked_mul(*b),
                _ => a.checked_rem(*b),
            };
            result.map(Value::Int).ok_or_else(|| {
                error(
                    code::ARITHMETIC,
                    pos,
                    format!("`{symbol}` overflowed a 64-bit int"),
                )
                .with_help("convert to float first: `float(x)`")
            })
        }
        _ => {
            let (Some(a), Some(b)) = (left.as_number(), right.as_number()) else {
                return Err(if op == BinaryOp::Add {
                    mismatch("two numbers, two strings or two lists")
                } else {
                    mismatch("two numbers")
                });
            };
            let result = match op {
                BinaryOp::Add => a + b,
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div => a / b,
                _ => a % b,
            };
            if result.is_finite() {
                Ok(Value::Float(result))
            } else {
                Err(error(
                    code::ARITHMETIC,
                    pos,
                    format!("`{symbol}` left the float range"),
                ))
            }
        }
    }
}
