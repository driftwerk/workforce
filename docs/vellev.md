# vellev: shadow mode and promotion

How a hub runs vellev beside Weaviate in shadow mode, how the shadow is
judged, and how vellev is promoted.

## Shadow mode for memories (DW-676)

**Known-item runs carry the memory verdict** (DW-676, PR 10). Live recalls
are too few for one (about 300 a week, against the 1,000 asked), so each
night, starting within `[vellev.shadow.known_items] hour_utc` to
`until_hour_utc` (02:00–06:00 UTC unless set; a hub that comes up later
waits for the next night), the leader draws up to `items` memories filed
under an identity (1,000 unless set; 0 turns the runs off) and up to
`unscoped_items` filed under none (250 unless set), from the Weaviate
collection that serves, and asks both stores for each one.

- **The draw is seeded.** It uses `seed` and the run's UTC day, so the same
  memories give the same set and the same queries. Each night draws afresh.
- **A memory is not drawn when** it is deleted, carries an audit `kind:`
  tag, was last written within two reconciles (counting a recurrence), or
  has fewer than 8 words. Each exclusion is counted on the run.
- **Memories filed under no identity** are their own population,
  recalled unscoped, as system jobs recall them. Rows and the summary keep
  the two populations apart.
- **Each memory is asked in two modes**: a window of half its words, and
  a hard query of 4 to 8 words from its middle. Neither is all of it.
- **Found** means the memory came back by its id, or as another memory of
  the same content (normalised alike). Finds by a duplicate are counted
  apart.
- **Limitation.** Every query is an excerpt of the memory it is to find,
  so it is easier than a live recall. The runs show whether the two
  indexes agree and rank alike, not real-world recall quality.
- **Each query is embedded once.** Both stores get that vector, at
  alpha 1.0 and at the serving alpha.
- **The runs keep the load down.**
  - Items go one at a time, `pace_ms` apart (250 unless set).
  - Each item holds one of `[vellev.shadow.memories] permits`, so the live
    samples keep the rest.
  - Nothing runs while the shadow is paused, and a draw stops when told
    to.
  - Settings whose items times `pace_ms` would reach six hours are
    refused at start-up.
  - Postgres claims one run a day, so two hubs never both run it. A day is
    tried at most three times, and a run left running for six hours is
    taken for abandoned.
- **Results** go to `shadow_known_item_runs` and `shadow_known_items`, kept
  180 days. They hold no query text.
  - An error is recorded as an error, never as a miss.
  - vellev failing 20 items in a row ends the run as `incomplete`.
  - `ShadowStore::known_item_summary` gives, per alpha and over the items
    both sides answered: how often each side put the memory first, MRR,
    recall within k, and the discordant pairs a paired (McNemar) test
    needs.
- **These runs are M2's pre-week replay.** The nights before day 1 are the
  replay.

**Artifacts get known-item runs too**, under `[artifacts] index_backend =
"shadow"`.

- **The draw.** Each night, under the same settings and seed rule, the
  leader draws artifacts from Postgres. It leaves out any updated within
  30 minutes and any too short.
- **Asking.** Each query, a window of the artifact's title and content
  words, goes to the serving Weaviate index and to the vellev mirror, one
  attempt each, at depth 10. The rows give recall@5, recall@10 and MRR.
- **When a run is made.** Only while the mirror is in step and the
  serving index is complete, as the read shadow requires. An artifact
  either index has yet to take (still pending) is not drawn. Each item holds
  one of `[vellev.shadow.artifacts] permits`. Below A14's minimum the
  report says "too few to judge".

**The LLM judge is opt-in** (`[vellev.shadow.judge] enabled`, off by
default).

- **What it judges.** For the artifacts' known-item queries only, so no
  user's query is kept, the tagger's LLM grades the pooled first five of
  both sides, 0 to 2. Each side's nDCG@5 is recorded.
- **Cost and reach.** It reads artifacts whatever their access rules, and
  each call is costed as the tagger's are (`shadow_judge`). A grading the
  LLM does not answer within 60 seconds is given up. It judges at
  most `daily_cap` queries a UTC day (200 unless set).
- **Off, it does nothing.** It is never made: no LLM is looked up and no
  artifact is read for it.

**Operating the shadow week** (DW-676, PR 9c).

- **`[vellev.shadow] paused` is read once, at start-up.** Changing it needs
  a restart of **every** hub replica: a replica not
  restarted keeps what it read. `workforce shadow pause` and `resume`
  change nothing; they print the config change and the restart.
  - Paused stops: the read comparisons, the memory mirror's noter and
    applier, the reconcile, and the known-item runs.
  - Paused does **not** stop erasure: a purge still reaches vellev's
    replica, in the same call (D1 (a)).
  - Paused does **not** stop the artifact mirror (its keeper and its
    noting of writes): vellev's artifact index is kept in step whether
    paused or not. Only the artifacts' read comparisons and known-item
    runs stop.
- **The CLI never builds or migrates the tables it reads.** `workforce
  shadow status` and `report` open them read only, and decline when no hub
  of this version has built them. A metrics scrape and the admin routes
  read only what their hub process already opened.
- **`workforce shadow status`** (or `GET /api/admin/shadow/status`) shows
  what is shadowed, whether paused, the memory mirror's backlog, and the
  weekly verdicts recorded. With `--hub URL` (and the gateway token, `--token`
  or `WORKFORCE_GATEWAY_TOKEN`) it adds that hub's `/api/health` shadow
  block. Those counts are **one process's**: where several replicas
  run, each counts only what it did.
- **`workforce shadow report --store memories|artifacts [--week
  YYYY-MM-DD]`** (or `GET /api/admin/shadow/report?store=&week=`) judges
  seven UTC days from Postgres alone, so any replica gives the same
  report. Every gate says `pass`, `fail`, `too few to judge` or
  `thresholds not set`; the last two are never a pass.
  - **Each day** is judged on M2's thresholds for that day's memory
    agreement, once it has at least 100 live recalls (or 100 items both
    answered in each gating group); below that the day's figures are
    information, and the day neither passes nor restarts the count. A day
    that fails restarts the count. A day's errors and empty answers are
    shown, for information.
  - **The week** is judged on:
    - volume;
    - L1: vellev's p50 and p99 at or under Weaviate's, over the week's raw
      comparisons, the timed out counted at their time;
    - errors and timeouts, and empty on vellev only: each passes when the
      one-sided 95 % Wilson upper bound of the week's rate is at most
      0.5 %. Too few comparisons for even none to show that (about 540)
      is too few to judge;
    - agreement (M2 thresholds, and on the known-item runs a self@1 test);
    - for artifacts, known-item recall@10 at most 2 points under
      Weaviate's, in window mode at alpha 1.0 over at least 300 items a
      week (other groups, overlap and RBO shown, not judged). With the
      judge off, nDCG@5 is "not judged".
  - **Memories' agreement** rests on the live recalls at 1,000 or more a
    week. Below that it rests on the known-item runs, and the report says
    so; with neither, it is too few to judge. Only a run that ran to its
    end counts, one a store and day.
    - **Two groups gate:** scoped memories and the `unscoped` population
      (the memories filed under no identity), each in window mode at the
      serving alpha, which is `[embeddings] alpha` as configured. A week
      whose runs were asked at another alpha (it changed mid-week) is too
      few to judge. Each group meets M2's thresholds and, over the week, a
      paired non-inferiority test of self@1. Take b, the items found first
      by Weaviate alone, and c, those found first by vellev alone, of the n
      pairs. The exact one-sided 95 % lower bound of (c - b) / n must be
      above minus the margin. That bound is Clopper-Pearson on c of b + c
      and on b + c of n, each at 97.5 %.
      - Fewer distinct memories than the minimum is too few to judge. A
        memory drawn on several nights counts once.
      - So is a week of pairs too few to show the margin even with none
        discordant: 736 at half a point.
    - **Reported, not judged:** the hard mode and alpha 1.0.
    - A find by a duplicate of the same content counts as found, and is
      shown apart.
  - **`[vellev.shadow.gates]`** holds M2's thresholds (`overlap`, `top1`,
    `rbo`: all three, each at or above its floor of 0.85, 0.85 and 0.80;
    unset, agreement says "thresholds not set"), and the self@1 test's
    `self_first_margin` (0.005, half a point, unless set; at most 0.05),
    `self_first_min` and `unscoped_min` (600 distinct memories a week each
    unless set).
  - **R1** is **not judged** for now. A fix is unexplained until a cause is
    recorded for it, and the reconcile records what it fixed, not why. The
    report shows the fixes of the week's last three days.
  - **Where vellev's time went.** Where rows carry PR 9b's labels, the
    report shows the queue and index-call p50 and p99, and vellev's time
    alone against in a burst. This is information, not a gate.
  - **Not judged here**, and listed as such in every report: the restore
    drill, the consistency checks, the backlog's history and the mirror's
    lag, and the kernel's busy and breaker drops (in `/api/metrics` only).
- **Weekly verdicts are kept.** Before the leader's retention pass deletes
  any comparison (after 14 days), it records the verdict of every whole
  ISO week (Monday to Sunday, UTC) in `shadow_verdicts`, kept 180 days. If
  it cannot, it deletes nothing that pass.
- **Metrics**, per hub process, to be summed across replicas:
  `workforce_shadow_kernel{store,count}` (offered, paused, unsampled,
  no_runtime, busy, breaker_open, started, ok, failed, panicked,
  timed_out), `workforce_shadow_enabled{store}`,
  `workforce_shadow_breaker_open{store}`,
  `workforce_shadow_memory_deferred_dropped` (memories' comparisons dropped
  before their sink was open), `workforce_shadow_sink{count}`, and the
  noter's `workforce_shadow_memory_notes_*` and
  `workforce_shadow_memory_noter_failed`. The backlog,
  `workforce_shadow_memory_backlog_pending` and `_oldest_secs`, is
  Postgres's, the same from any replica, read on the shadow tables'
  pool the hub already holds; when it cannot be read within 2 s, only `workforce_shadow_memory_backlog_unknown 1` is given, never 0.
  `/api/health` carries the same process counts under `shadow`, but not
  the backlog: health asks nothing of Postgres.

## Promoting vellev (DW-676)

Promotion makes vellev the store that serves memories, or the artifact
index, after a shadow week has passed. `workforce shadow promote` does
it, and `workforce shadow rollback` undoes it. Both run **with every hub
stopped**. A hub still on `shadow` keeps writing memories to Weaviate, and
none of those writes would reach a promoted vellev.

Promotion takes the replica's lock alone, records the promotion in
Postgres (`shadow_memory_promotion`), then writes vellev's promotion
marker. Either record stops every replica write. Postgres's comes first
because it survives a restore of the tenant to an earlier point, which can
take vellev's away.

### What the commands check

`workforce shadow promote memories`, run on the shadow config:

1. **No hub is running.** No process holds an artifact index's serving
   lock, which every hub's keeper holds for as long as it runs. The
   artifact mirror's lease has also lapsed, which takes 90 s after the
   last shadow hub stops.
   - The check proves nothing behind PgBouncer in transaction mode.
   - Checking that no hub task is running (R-1) is the backstop.
2. **The week's verdict is a pass.**
   - *This release reads no recorded verdict.* The weekly verdicts come
     with the operator report (PR 9c), so until then every promotion is
     refused here.
   - The verdict must cover every gate of design r2 §8.1, the restore
     drill among them.
3. **A last reconcile** brings the replica in line. It is recorded as any
   reconcile is.
4. **The notes it covered are taken away.** The backlog must then be zero.
   A note left means something still writes memories.
5. **A second whole pass must fix nothing.** It is recorded too. The
   start-up guard reads it as the last check.
6. **The collection is marked** as holding every memory, if it was not
   already (it was not if the reconcile seeded it rather than an import).
7. **The promotion is written.** Postgres's record goes first, then
   vellev's marker, under the replica's lock held alone.

`workforce shadow promote artifacts` makes the same checks of the hubs
and the verdict. Then:
- It fills vellev's mirror from Postgres, and a second fill must find
  nothing to do, with the index marked complete.
- It ends the mirror's noting: first the lease, set to `-infinity` (what
  says the index was promoted, in Postgres), then the leftover notes.
- It marks the index promoted.
- A "too few to judge" verdict (A14) is accepted only with
  `--accept-too-few`, which is the owner's sign-off. It is never accepted
  for memories.

### What a hub refuses at start-up

- **`[memory] backend = "vellev"`, where memories were shadowed.** Refused
  unless they are promoted, with:
  - no note left for the replica;
  - the collection marked;
  - the last check clean: the promotion is recorded only after a clean
    pass, and any reconcile recorded since must be `ok` with nothing
    fixed. (The reconciles' rows are pruned after 180 days; the record
    stands for the check once they are.)
- **`"weaviate"` or `"shadow"`, once memories are promoted.** Refused:
  Weaviate lacks what was written since. Roll back first.
- **`[artifacts] index_backend = "vellev"`.** Refused while a shadow of
  the index is leased or noted, or was shadowed and is not promoted.
- **Facts that cannot be read.** When Postgres or vellev cannot be asked,
  the hub refuses rather than starting on facts it could not read. Every
  process that opens memories asks, the CLI and the MCP server too.
- **No shadow ever run here.** An install that never ran a shadow on this
  database (no reconcile recorded, no note, no promotion) opens vellev as
  before.

### Before the window: `WorkforceMemory` goes first (P-1 to P-4)

Weaviate's `WorkforceMemory` is the collection memories switched from.
- The hub keeps it beside the one it serves by default (`[embeddings]
  keep_previous`): it catches up new memories from it for half an hour
  after each start, and carries changes back into it after that.
- The edge (`workforce-edge`) keeps its customers' memories there too.

Once the hub leaves it alone, it misses every erasure. A class kept
frozen like that, holding erased memories, is the gap these steps close:
it is dropped as soon as nothing needs it, never kept as a stale copy.

Do these in order, each finished before the next, well before the window.

- **P-1. `[embeddings] keep_previous = false` on every hub, rolled out.**
  - From then on nothing of the hub's reads or writes `WorkforceMemory`.
  - Postgres records for good that the hub left it alone
    (`shadow_memory_history`). From then on every hub on `weaviate` or
    `shadow` refuses to start with the key back on, and so do
    `promote memories` and `rollback memories`.
  - It must be live before P-2, so that no hub catches the edge's
    memories back up while they move.
- **P-2. The edge moves to a store of its own**: copy its memories,
  switch the edge to them, then purge them from `WorkforceMemory` and from
  the copies the hub's catch-up made of them in the hub's stores.
- **P-3. Checks before the drop. All must hold:**
  - **The edge has moved.** Its config names its own store, it serves
    from it, and no object in `WorkforceMemory` has an edge agent's id as
    its owner.
  - **The counts agree.** Every memory id in `WorkforceMemory` is held by
    the hub's bge class, the store it serves from: it holds nothing the
    hub would lose. Count both and list any id the bge class lacks. Any
    such id is looked at before the drop; none is dropped unseen.
  - **A backup.** A backup of Weaviate's volume or class is taken and its
    checksum recorded. It is the last copy, and it is kept for the
    backups' usual retention, as every backup of erased memories is.
- **P-4. Drop `WorkforceMemory`** straight after P-3. Nothing reads it, so
  there is nothing to wait for.

### Before the window (R-0)

- P-1 to P-4 are done: `keep_previous = false` is live on every hub, the
  edge has moved, and `WorkforceMemory` is gone.
- The weekly verdicts for both stores are recorded as passing, and the
  restore drill (below) was clean.
- The node's memory limit fits: 2.2 times its measured resident size
  must fit under the limit the node actually runs with.
  - Once vellev serves, an OOM kill is an outage, not a lost comparison.
- Alerts on the node's memory, restarts and availability route to
  someone.
- The node's own Postgres is dumped nightly, beside the hub's. It holds
  the tenant's restore points.
- The daily vellev backup has succeeded within 24 hours.
- Weaviate's nightly backup is fresh. It is the last copy of Weaviate's
  memory class after R-5.

### The window

- **R-1.** Stop every hub, and keep them stopped until R-4. Stop any other
  `workforce` process that writes memories (an MCP server, a CLI, a cron
  job).
  - Check with your orchestrator that **no hub task is running or
    starting**, and on each host that no `workforce` process is left.
  - Wait 90 s for the mirror's lease to lapse.
  - This check is the one that counts. `promote` also refuses while a hub
    holds an artifact index's serving lock, and holds those locks itself
    to its end, so that a hub started meanwhile serves no index. But that
    proves nothing behind PgBouncer in transaction mode, nor of a process
    that keeps no artifact index.
- **R-2.** From a one-off process that can reach Postgres and the node,
  with the **shadow** config:
  ```sh
  workforce shadow promote memories --config workforce.toml
  workforce shadow promote artifacts --config workforce.toml   # --accept-too-few only on the owner's sign-off
  ```
  If either refuses, put right what it names and run it again.
  - **One exception.** A memory promotion stopped between Postgres's record
    and vellev's marker (a crash, or the node down at that moment) cannot
    be finished by running it again: the record already refuses every write
    into the replica, so the last reconcile cannot run. `promote` says so.
    Recover with `workforce shadow rollback memories`, then `promote`
    again. The same applies if a restore of the tenant took the marker
    away.
- **R-3.** Set `[memory] backend = "vellev"` and `[artifacts]
  index_backend = "vellev"`. Leave `[vellev.shadow]` as it is: it is read
  only for `shadow`. `[weaviate]` stays enabled, for code vectors.
- **R-4.** Start the hubs, then check:
  - no promotion refusal in the logs;
  - a recall and an artifact search answer;
  - `/api/health` shows the artifact index in step.
- **R-5.** Drop Weaviate's bge memory class
  (`WorkforceMemory_bge_base_en_v1_5`) and its `Artifacts` class.
  - Nothing reads them once memories and artifacts are promoted.
  - A memory erased on vellev from then on would otherwise stay in them,
    and erasure that leaves a copy behind is not erasure.
  - A rollback re-makes the memory class from vellev.
  - Action no erasure request between R-4 and R-5.

After R-5, nothing of the hub's memories, the artifact index or the edge
reads Weaviate (`WorkforceMemory` went at P-4). Track 2's window, which
stops Weaviate, may follow.

### Rolling back

Stop every hub, as in R-1. On the shadow config the hubs will start on
again:

```sh
workforce shadow rollback memories --config workforce.toml
workforce shadow rollback artifacts --config workforce.toml
```

- **Memories.** Everything written on vellev since the promotion is
  carried back into Weaviate: new memories, edits, forgetting and
  erasures.
  - Weaviate's class is made again first if R-5 dropped it. Vectors are
    copied, not made again.
  - A second pass must find nothing to fix.
  - vellev's first collection (`WorkforceMemory`), which a hub serving
    from vellev makes empty, is dropped, because the shadow refuses a
    tenant holding it. One that holds anything is never dropped: the
    rollback refuses instead.
  - The marker goes, then the record.
- **Artifacts.** Only the marker goes, because Postgres holds every
  artifact. Weaviate's keeper compares its index when it first serves
  again, and puts it right. Until then a search of an index out of step
  goes by keyword.

Then start the hubs on `shadow`. The reconcile resumes.

### The restore drill (A17)

The drill runs during the shadow week, before promotion, at a quiet hour,
never while the tenant is being seeded, and never in the promotion window.
1. Record the restore points and the counts of both collections. Take a
   backup (`vellev backup --tenant <tenant> take`) and list it.
2. Restore to about an hour back: `vellev backup --node <node URL>
   --tenant <tenant> restore --at <time>`. If the outcome is unknown,
   retry under the same name.
3. Let the reconcile (every 15 minutes) and the artifact mirror's first
   compare converge. The check must then be clean: a reconcile that ends
   `ok`, then one that fixes nothing.
4. Optionally, `restore --backup <id>`, then the same check.
5. Throughout, watch the working set against the node's limit. A restore
   peaks at about 2.15 times the tenant's loaded size. If the measured
   resident size times 2.2 exceeds the limit, the owner raises it **before**
   the drill, and the drill's record says so.

**After promotion a restore loses writes.** vellev is then the store that
serves, and a point-in-time restore drops everything written after its
restore point. Before restoring a promoted tenant, either:
- roll back first, so that Weaviate takes what vellev holds; or
- the owner accepts the loss, in writing.

### Notes for whoever runs it

- **`keeper_stalled`** in health does not by itself mean the artifact
  mirror's keeper is broken. It means no step finished within its stall
  bound, about 120 s. A slow vellev does that too. Check vellev's latency
  and the keeper's logs before restarting anything.
- **`[vellev.shadow] paused` is read at start-up.** A change needs every
  hub restarted.
  - For memories it stops the read comparisons, the noter, the
    reconcile and the applier. The replica then falls behind until the
    pause ends. Promotion runs its own reconcile, paused or not.
  - For artifacts it stops only the read comparisons. The mirror's noting
    and its keeper go on.
  - It never stops erasure.
  - It does nothing once a store is promoted.

## The edge's own memory store (DW-676 (b))

The customer-facing edge once kept its memories in the hub's Weaviate
class, `WorkforceMemory`. The hub's switch to its own embeddings copied
them into the hub's bge class, and the shadow into vellev's replica of
that. An edge customer's erasure purged `WorkforceMemory` alone, so the
copies outlived it; and the hub's carry-back (bge class into
`WorkforceMemory`, every 2 minutes while `[embeddings] keep_previous` is
on) could put some of those erased memories back into `WorkforceMemory`
itself. The edge now keeps its memories in a vellev tenant of its own, on
the node that holds the hub's memories, and its erasure purges there.

**The edge's config refuses to share again.** A `[weaviate]` section must
name `collection`, and never the hub's (`WorkforceMemory`, or any name
starting so). The edge does not start otherwise. `[vellev]` gains
`query_prefix`, for a model that wants one (bge's), and `score_map`,
required with a bge model (the edge refuses to start without it): knots
putting the node's certainty on the scale the edge's recall floors
(memories 0.45, lessons, knowledge 0.7) were tuned on, MiniLM's, as the
hub's own `[embeddings] score_map` does for bge.

**The edge's tenant** embeds with the node's own model,
`BAAI/bge-base-en-v1.5` (768 dimensions, query prefix
`Represent this sentence for searching relevant passages: `), so that the
node loads no second model, with the same `score_map` knots as the hub's.

**`workforce memory move-edge`** moves what the edge left in the hub's
stores, in two steps, each a dry run without `--apply` and safe to run
again. Both refuse unless the config it is run with has `[embeddings]
keep_previous = false`; that must also be true of every running hub
replica, or a carry-back brings erased memories back and races the purge.

- `copy`, with the edge stopped: every object of `WorkforceMemory` whose
  owner is one of `--agent` and whose identity is one the edge writes
  under (a customer's `ext:` key, `edge:lessons`, `edge:knowledge`) is
  copied into the edge's tenant under its own id, except a customer's
  memory whose customer is not in `--known-customers` (the edge erased
  them: it is purged, never copied). Every copy is read back and checked,
  and only then is `edge-memories-moved-from-weaviate` marked there. Once
  marked, `copy` is refused: the edge then writes there, and copying again
  would bring back what a customer erased.
- `purge`, refused until the copy is marked: every memory an edge object
  holds in any of the three stores is purged from all of them; vellev's
  replica and the bge class together under the replica's lock, as any of
  the hub's erasures, then `WorkforceMemory`. It then looks again, and
  fails if any is still held.

An edge object found only in the bge class or the replica is a copy whose
original the edge erased: it is purged, never copied. A purge is by memory
id, so a memory also held by an object that is not the edge's refuses the
whole step before anything is written. The command prints counts and ids,
never content. It refuses an `--agent` the hub runs (`[[agents]]`, a team
member, or a workspace under `data_dir/agents`, so wherever it runs must
see the hub's data directory), an edge tenant that is the hub's (compared
ignoring case), a promoted replica, and any `[memory] backend` but
`"shadow"`.

### Deploy order

1. **Turn off the hub's previous store**: `[embeddings] keep_previous =
   false` on every hub, and every hub replica restarted on it (each logs
   that it no longer keeps the previous collection). Nothing below starts
   before that.
2. **Make the edge's tenant** with the node's admin key:
   `PUT /v1/tenants/<edge tenant>`, then
   `POST /v1/tenants/<edge tenant>/keys`. Keep the key it returns in the
   edge's secrets as `EDGE_VELLEV_API_KEY`. If the node restricts its
   tenants (`VELLEV_TENANTS`), add the edge's tenant to the list first.
3. **Stop the edge**, in a way the next deploy does not undo. The new
   release would not start on an `edge.toml` that names no collection
   anyway.
4. **Deploy** the release with this change (hub and edge).
5. **List the customers the edge still knows**, from its database:

   ```text
   psql "$EDGE_PG_URL" -At -c 'SELECT owner FROM edge_analytics.chat_identities' > known.txt
   ```

   An erasure deletes a customer's row there; a customer with memories and
   no row was erased, and their memories are purged, never copied. The
   file holds customers' identifiers: keep it where the edge's data is
   kept, and delete it after step 9.
6. **Copy**, with the hub's config, the hub's data directory visible (for
   the agent check), `known.txt`, and the edge's tenant key in the
   environment:

   ```text
   workforce memory move-edge copy --config workforce.toml \
     --agent <edge agent id> --edge-url <node URL> --edge-tenant <edge tenant> \
     --edge-model BAAI/bge-base-en-v1.5 --edge-dimensions 768 \
     --edge-query-prefix 'Represent this sentence for searching relevant passages: ' \
     --known-customers known.txt
   ```

   Read the counts, then run it again with `--apply`. If the dry run shows
   customer memories of customers the edge no longer knows, find out why
   before `--apply`: such a customer may predate `chat_identities`, and
   their memories would be purged, never copied.
7. **Switch the edge's config**: replace `[weaviate]` in `edge.toml` with

   ```toml
   [vellev]
   url = "<node URL>"
   api_key = "${EDGE_VELLEV_API_KEY}"
   tenant = "<edge tenant>"
   model = "BAAI/bge-base-en-v1.5"
   dimensions = 768
   query_prefix = "Represent this sentence for searching relevant passages: "
   # The same knots as the hub's [embeddings] score_map for bge-base.
   score_map = [[0.0, 0.0], [0.72, 0.60], [0.77, 0.65], [0.80, 0.70], [0.83, 0.80], [0.935, 0.92], [0.98, 0.97], [1.0, 1.0]]
   connect_budget_secs = 300
   ```

   and give the edge `EDGE_VELLEV_API_KEY`.
8. **Start the edge** again, and check that a known customer's memory is
   recalled.
9. **Purge**: the same command with `purge` (no `--known-customers`),
   dry, then `--apply`. Then run `purge` once more without `--apply`: it
   must find nothing.

`WorkforceMemory` may be dropped only after step 9.
