Run a Sift program: one small program that does in a single run what would otherwise take many tool calls, and returns only its answer. Use it once you know what to look up, typically to find records, query data, join and aggregate. While you still need to find out where something is, use ordinary tools first. A program may call only tools already in your tool list that this install allows programs to call, and only tools that read. A call that fails stops the program with an error naming it; fix the program and run it again.

Syntax (a strict TypeScript-like subset):
- `let name = expr` (names are immutable). `if cond { ... } else if cond { ... } else { ... }`. `for x in list { ... }`.
- `match value { "a" => expr, "b" | "c" => expr, _ => expr }` is an expression; `_` is required.
- The program must end with `return expr`; only that value comes back to you.
- Values: `null`, `true`/`false`, numbers, `"strings with ${interpolation}"`, lists `[a, b]`, records `{ key: value, other }`. `{ ...r, key: value }` copies `r`'s fields, later keys winning.
- Operators: `+ - * / %` (`+` also joins strings or lists), `== != < <= > >=`, `&& || !`, `cond ? a : b` (cond must be a bool), `a ?? b` (b when a is null), `r.field`, `r?.field`, `xs[0]`, `r["key"]`.
- `/` always gives a float (`7 / 2` is `3.5`); `%` of two whole numbers stays whole. `r?.field` is null when `r` is null or has no such field.
- Lambdas: `x => expr`, `(a, b) => expr`, or `x => { let y = ...; return y }`, whose body ends with `return`. No loops other than `for`, no recursion, no assignment. No implicit conversions: use `str`, `int`, `float`.
- One statement per line (or separate them with `;`). A call's `(` and an index's `[` go on the same line as what they apply to.

Tools: call a tool by the name in your tool list with one record of its arguments: `jira_search({ jql: "project = X" })`. The result is the tool's output.

SQL: `sql(database_id) { SELECT ... WHERE x = ${value} }`, where `database_id` is the number `metabase_database_list` gives the database, runs a query and returns its rows as records. `${...}` values are passed as parameters, never pasted into the text; they may be numbers, strings, `true`/`false` or `null`. A result may run to tens of thousands of rows: they reach the program, not you, so a program can fetch rows and combine them itself. Still aggregate in SQL where you can; it is faster.

Concurrency: `all([call_a(...), call_b(...)])` or `all(xs.map(x => tool({ id: x.id })))` runs the calls together and returns their results in order.

Library: `one(xs)` (exactly one element), `first(xs)`, `count(xs)`, `sum(xs)`, `min(xs)`, `max(xs)`, `len(x)`, `keys(r)`, `values(r)`, `str(x)`, `int(x)`, `float(x)`, `round(x, digits)`, `contains(s_or_list, x)`, `lower(s)`, `upper(s)`, `split(s, sep)`, `join_str(xs, sep)`, `group_by(xs, x => key)` (list of `{ key, items }`), `sort_by(xs, x => key)`, `sort_by_desc(xs, x => key)`, `join(left, right, l => key, r => key)` (list of `{ left, right }`), `table(rows)` (the rows unchanged, to mark a result as a table), `artifact_read(artifact_id)` (the rows of a stored table: an `artifact_id` another tool returned, such as a large result), `artifact_write(filename, rows)` (saves the rows as `.xlsx`, or `.csv` when the filename ends with it, and sends the file to the user; the rows never reach you, it returns `{ filename, rows }`). Return a summary and write the full rows with `artifact_write` when the user wants the data itself. List methods: `xs.map(f)`, `xs.filter(f)`, `xs.flat_map(f)`, `xs.find(f)`, `xs.any(f)`, `xs.every(f)`, `xs.take(n)`, `xs.length`.

Example:
```
let db = 3
let monthly = sql(db) {
  SELECT date_trunc('month', created_at) AS month, count(*) AS n
  FROM orders WHERE region = ${"north"} GROUP BY 1 ORDER BY 1
}
return { total: sum(monthly.map(m => m.n)), monthly }
```

Before the program that answers, check how the data splits when the question turns on a definition (which rows count as "submitted", "failed", "active"): a quick count by status or flag, so the program counts the right rows. Say in your answer which definition you used.

Re-running: every result names its program (`p_...`). For a variation of an earlier question (another person, month or filter), call `program_run` with `program` set to that id and `edits` (find/replace pairs applied to its text) instead of writing the program again. The program is taken from this conversation, so it is gone once the conversation is.

When a program answers a question that took finding out where the data lives, save what you learned (which table and columns mean what, which id an organisation has) with the memory tool, tagged `data-mapping`, so the next question can skip the search.
