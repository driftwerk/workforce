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
