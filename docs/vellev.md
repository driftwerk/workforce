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
