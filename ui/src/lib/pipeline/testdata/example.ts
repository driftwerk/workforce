/**
 * An example orders board as the hub read it at 06:19 UTC on 15 Sep 2026,
 * reduced to what the definition reads, under revision 10 of its definition
 * with revision 11's windows on the columns where an item is finished: every
 * event the picker schedules carries `due_at` as item-picker 0.9.0 records it.
 * ORD-2601 and ORD-2655 finished weeks earlier, so a week's window hides them.
 */

import type { PipelineDefinition } from "../definition";
import type { LineageBody } from "../lineage";

export const orders: PipelineDefinition = {
  "name": "Orders",
  "description": "Orders from intake to a verified release: picked, implemented, reviewed, merged, deployed and checked for a week.",
  "item_label": "ticket",
  "collection": "pipeline",
  "revision": 10,
  "sources": [
    "workflow:order-intake",
    "workflow:order-review",
    "workflow:order-release",
    "workflow:order-verify",
    "skill:item-picker"
  ],
  "lanes": [
    {
      "id": "implementing",
      "label": "Implementing"
    },
    {
      "id": "pr-open",
      "label": "PR open"
    },
    {
      "id": "reviewing",
      "label": "Reviewing"
    },
    {
      "id": "reviewed",
      "label": "Reviewed"
    },
    {
      "id": "merging",
      "label": "Merging"
    },
    {
      "id": "deploying",
      "label": "Deploying"
    },
    {
      "id": "deployed",
      "label": "Deployed"
    },
    {
      "id": "verifying",
      "label": "Verifying"
    },
    {
      "id": "verified",
      "label": "Verified",
      "window": "7d"
    }
  ],
  "exits": [
    {
      "id": "not-code",
      "label": "Not a code change",
      "window": "7d"
    },
    {
      "id": "released",
      "label": "Needs a person"
    },
    {
      "id": "sent-back",
      "label": "Sent back · self-healing"
    },
    {
      "id": "duplicate",
      "label": "Duplicate",
      "window": "7d"
    },
    {
      "id": "blocked",
      "label": "Review blocked"
    },
    {
      "id": "human-merge",
      "label": "Awaiting a human merge"
    },
    {
      "id": "deploy-blocked",
      "label": "Deploy blocked · needs a person"
    },
    {
      "id": "not-fixed",
      "label": "Not fixed"
    },
    {
      "id": "regression",
      "label": "Regression"
    },
    {
      "id": "inconclusive",
      "label": "Inconclusive · needs a person"
    },
    {
      "id": "not-live",
      "label": "Not live · self-healing"
    }
  ],
  "rungs": [
    "1h",
    "6h",
    "24h",
    "72h",
    "168h"
  ],
  "events": {
    "implement/claimed": [
      {
        "column": "implementing",
        "tone": "active",
        "claim": true
      }
    ],
    "implement/pr-opened": [
      {
        "column": "pr-open"
      }
    ],
    "implement/released": [
      {
        "when": {
          "label": "stage-skipped"
        },
        "column": "not-code"
      },
      {
        "column": "released",
        "tone": "warn"
      }
    ],
    "implement/retry": [
      {
        "column": "sent-back",
        "tone": "healing",
        "label": "another attempt follows"
      }
    ],
    "implement/sent-back": [
      {
        "column": "sent-back",
        "tone": "healing"
      }
    ],
    "review/claimed": [
      {
        "column": "reviewing",
        "tone": "active",
        "claim": true
      }
    ],
    "review/resumed": [
      {
        "column": "reviewing",
        "tone": "active",
        "label": "resumed after the run was interrupted",
        "claim": true
      }
    ],
    "review/sent-back": [
      {
        "column": "sent-back",
        "tone": "healing",
        "label": "sent back for another attempt"
      }
    ],
    "review/finished": [
      {
        "when": {
          "verdict": "stage-review-blocked"
        },
        "column": "blocked",
        "tone": "bad"
      },
      {
        "column": "reviewed",
        "tone": "good"
      }
    ],
    "merge/gate": [
      {
        "when": {
          "outcome": "human"
        },
        "column": "human-merge",
        "tone": "warn"
      },
      {
        "when": {
          "outcome": "already-merged"
        },
        "column": "deploying"
      },
      {
        "column": "merging",
        "tone": "active"
      }
    ],
    "merge/auto-merged": [
      {
        "column": "deploying",
        "label": "Merged"
      }
    ],
    "deploy/claimed": [
      {
        "column": "deploying",
        "tone": "active",
        "claim": true
      }
    ],
    "deploy/retry": [
      {
        "column": "deploying",
        "tone": "healing",
        "label": "deploy not confirmed, retrying"
      }
    ],
    "deploy/finished": [
      {
        "when": {
          "label": "stage-deploy-blocked"
        },
        "column": "deploy-blocked",
        "tone": "bad"
      },
      {
        "column": "deployed",
        "tone": "good",
        "due_field": "due_at"
      }
    ],
    "verify/started": [
      {
        "column": "verifying",
        "tone": "active",
        "rung_field": "rung",
        "label": "checking a rung",
        "claim": true,
        "lease_field": "lease_minutes"
      }
    ],
    "verify/rung": [
      {
        "when": {
          "state": "pass"
        },
        "column": "verifying",
        "tone": "active",
        "rung_field": "rung",
        "rung_passed": true,
        "due_field": "due_at"
      },
      {
        "column": "verifying",
        "tone": "active",
        "rung_field": "rung",
        "due_field": "due_at"
      }
    ],
    "verify/verified": [
      {
        "column": "verified",
        "tone": "good",
        "rung_field": "rung",
        "rung_passed": true
      }
    ],
    "verify/failed": [
      {
        "when": {
          "state": "regression"
        },
        "column": "regression",
        "tone": "bad"
      },
      {
        "column": "not-fixed",
        "tone": "bad"
      }
    ],
    "verify/inconclusive": [
      {
        "when": {
          "final": "true"
        },
        "column": "inconclusive",
        "tone": "warn",
        "rung_field": "rung"
      },
      {
        "column": "verifying",
        "tone": "healing",
        "rung_field": "rung",
        "label": "too rare to judge yet; a later rung judges again",
        "due_field": "due_at"
      }
    ],
    "verify/not-live": [
      {
        "column": "not-live",
        "tone": "healing",
        "rung_field": "rung",
        "due_field": "due_at"
      }
    ],
    "verify/released": [
      {
        "column": "verifying",
        "tone": "healing",
        "rung_field": "rung",
        "label": "not judged this run; the rung waits for a later one",
        "due_field": "due_at"
      }
    ],
    "duplicate/closed": [
      {
        "column": "duplicate"
      }
    ]
  },
  "summary_field": "stages.implement.summary",
  "link_fields": [
    {
      "field": "pr_url",
      "label": "PR"
    }
  ],
  "timings": {
    "origin": {
      "field": "filed_at",
      "label": "Filed"
    },
    "milestones": [
      "deployed",
      "verified"
    ],
    "ends": [
      "verified",
      "not-code",
      "duplicate"
    ]
  }
};

/** Each record with the column the hub placed it in when the board was read. */
export const records: { id: string; column: string; body: LineageBody }[] = [
  { "id": "ORD-2814", "column": "deployed", "body": {"item": "ORD-2814", "stage": "deploy", "created_at": "2026-09-14T19:00:03.118478Z", "updated_at": "2026-09-15T05:19:32.014096Z", "stages": {"deploy": {"at": "2026-09-15T05:19:32.014096Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T19:35:55.043952Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T20:39:51.439004Z", "last_event": "gate"}, "review": {"at": "2026-09-14T20:39:52.727192Z", "last_event": "finished"}}, "history": [
    {"at": "2026-09-14T19:00:03.118478Z", "stage": "implement", "event": "claimed", "run": "ebf002f9", "detail": {}},
    {"at": "2026-09-14T19:35:55.043952Z", "stage": "implement", "event": "pr-opened", "run": "ebf002f9", "detail": {}},
    {"at": "2026-09-14T19:43:03.397494Z", "stage": "review", "event": "claimed", "run": "ab3a2605", "detail": {}},
    {"at": "2026-09-14T20:39:51.439004Z", "stage": "merge", "event": "gate", "run": "ab3a2605", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T20:39:52.727192Z", "stage": "review", "event": "finished", "run": "ab3a2605", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-15T05:16:02.888612Z", "stage": "deploy", "event": "claimed", "run": "b097322d", "detail": {}},
    {"at": "2026-09-15T05:19:32.014096Z", "stage": "deploy", "event": "finished", "run": "b097322d", "detail": {"label": "stage-deployed", "due_at": "2026-09-15T06:19:32Z"}},
  ] } },
  { "id": "ORD-2803", "column": "verifying", "body": {"item": "ORD-2803", "stage": "verify", "created_at": "2026-09-14T11:45:03.209837Z", "updated_at": "2026-09-15T05:19:05.156304Z", "stages": {"deploy": {"at": "2026-09-14T19:13:26.461613Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T12:46:37.726530Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T13:27:07.528999Z", "last_event": "gate"}, "review": {"at": "2026-09-14T13:27:11.052671Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T05:19:05.156304Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-14T11:45:03.209837Z", "stage": "implement", "event": "claimed", "run": "ae6c110a", "detail": {}},
    {"at": "2026-09-14T11:52:11.289854Z", "stage": "implement", "event": "released", "run": "ae6c110a", "detail": {"label": "stage-skipped"}},
    {"at": "2026-09-14T12:15:03.276127Z", "stage": "implement", "event": "claimed", "run": "c27b0008", "detail": {}},
    {"at": "2026-09-14T12:46:37.726530Z", "stage": "implement", "event": "pr-opened", "run": "c27b0008", "detail": {}},
    {"at": "2026-09-14T12:53:04.708085Z", "stage": "review", "event": "claimed", "run": "c2a1355f", "detail": {}},
    {"at": "2026-09-14T13:27:07.528999Z", "stage": "merge", "event": "gate", "run": "c2a1355f", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T13:27:09.521733Z", "stage": "review", "event": "follow-ups", "run": "c2a1355f", "detail": {}},
    {"at": "2026-09-14T13:27:11.052671Z", "stage": "review", "event": "finished", "run": "c2a1355f", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T15:21:03.567511Z", "stage": "deploy", "event": "claimed", "run": "aff3a529", "detail": {}},
    {"at": "2026-09-14T15:23:05.801828Z", "stage": "deploy", "event": "retry", "run": "aff3a529", "detail": {}},
    {"at": "2026-09-14T15:53:06.269458Z", "stage": "deploy", "event": "retry", "run": "dcf2e807", "detail": {}},
    {"at": "2026-09-14T16:08:10.784500Z", "stage": "deploy", "event": "retry", "run": "2085b999", "detail": {}},
    {"at": "2026-09-14T16:13:04.847647Z", "stage": "deploy", "event": "finished", "run": "ea3b6b1d", "detail": {"label": "stage-deploy-blocked"}},
    {"at": "2026-09-14T19:13:26.461613Z", "stage": "deploy", "event": "finished", "run": "5471c8a2", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T20:13:26Z"}},
    {"at": "2026-09-15T05:13:01.202967Z", "stage": "verify", "event": "started", "run": "cc3b1f33", "detail": {"lease_minutes": 150, "rung": "1h"}},
    {"at": "2026-09-15T05:19:05.156304Z", "stage": "verify", "event": "inconclusive", "run": "cc3b1f33", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-15T01:13:26Z"}},
  ] } },
  { "id": "ORD-2813", "column": "reviewed", "body": {"item": "ORD-2813", "stage": "review", "created_at": "2026-09-14T18:00:04.281152Z", "updated_at": "2026-09-15T05:14:35.880803Z", "stages": {"implement": {"at": "2026-09-14T18:31:21.555763Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T19:05:08.641440Z", "last_event": "gate"}, "review": {"at": "2026-09-15T05:14:35.880803Z", "last_event": "finished"}}, "history": [
    {"at": "2026-09-14T18:00:04.281152Z", "stage": "implement", "event": "claimed", "run": "37561799", "detail": {}},
    {"at": "2026-09-14T18:31:21.555763Z", "stage": "implement", "event": "pr-opened", "run": "37561799", "detail": {}},
    {"at": "2026-09-14T18:33:02.838002Z", "stage": "review", "event": "claimed", "run": "9042ba11", "detail": {}},
    {"at": "2026-09-14T19:05:08.641440Z", "stage": "merge", "event": "gate", "run": "9042ba11", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T19:05:09.882430Z", "stage": "review", "event": "finished", "run": "9042ba11", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-15T05:13:02.858112Z", "stage": "review", "event": "claimed", "run": "c8e795d2", "detail": {}},
    {"at": "2026-09-15T05:14:35.880803Z", "stage": "review", "event": "finished", "run": "c8e795d2", "detail": {"verdict": "stage-reviewed"}},
  ] } },
  { "id": "ORD-2815", "column": "deploying", "body": {"item": "ORD-2815", "stage": "deploy", "created_at": "2026-09-14T19:30:03.235358Z", "updated_at": "2026-09-15T05:11:03.816042Z", "stages": {"deploy": {"at": "2026-09-15T05:11:03.816042Z", "last_event": "claimed"}, "implement": {"at": "2026-09-14T20:05:33.310758Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T20:39:44.873363Z", "last_event": "gate"}, "review": {"at": "2026-09-14T20:39:48.053276Z", "last_event": "finished"}}, "history": [
    {"at": "2026-09-14T19:30:03.235358Z", "stage": "implement", "event": "claimed", "run": "d8c0440e", "detail": {}},
    {"at": "2026-09-14T20:05:33.310758Z", "stage": "implement", "event": "pr-opened", "run": "d8c0440e", "detail": {}},
    {"at": "2026-09-14T20:13:03.748192Z", "stage": "review", "event": "claimed", "run": "021ab67b", "detail": {}},
    {"at": "2026-09-14T20:39:44.873363Z", "stage": "merge", "event": "gate", "run": "021ab67b", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T20:39:46.746829Z", "stage": "review", "event": "follow-ups", "run": "021ab67b", "detail": {}},
    {"at": "2026-09-14T20:39:48.053276Z", "stage": "review", "event": "finished", "run": "021ab67b", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-15T05:11:03.816042Z", "stage": "deploy", "event": "claimed", "run": "b90b08c3", "detail": {}},
  ] } },
  { "id": "ORD-2807", "column": "verifying", "body": {"item": "ORD-2807", "stage": "verify", "created_at": "2026-09-14T14:00:04.165184Z", "updated_at": "2026-09-15T05:08:01.309654Z", "stages": {"deploy": {"at": "2026-09-14T19:10:37.745477Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T15:11:51.605102Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T15:52:27.097186Z", "last_event": "gate"}, "review": {"at": "2026-09-14T15:52:29.902786Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T05:08:01.309654Z", "last_event": "started"}}, "history": [
    {"at": "2026-09-14T14:00:04.165184Z", "stage": "implement", "event": "claimed", "run": "4eb70f2f", "detail": {}},
    {"at": "2026-09-14T15:11:51.605102Z", "stage": "implement", "event": "pr-opened", "run": "4eb70f2f", "detail": {}},
    {"at": "2026-09-14T15:13:06.457108Z", "stage": "review", "event": "claimed", "run": "8bfc8d1b", "detail": {}},
    {"at": "2026-09-14T15:52:27.097186Z", "stage": "merge", "event": "gate", "run": "8bfc8d1b", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T15:52:28.721007Z", "stage": "review", "event": "follow-ups", "run": "8bfc8d1b", "detail": {}},
    {"at": "2026-09-14T15:52:29.902786Z", "stage": "review", "event": "finished", "run": "8bfc8d1b", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T18:31:04.158933Z", "stage": "deploy", "event": "claimed", "run": "b0a6496f", "detail": {}},
    {"at": "2026-09-14T19:10:30.741140Z", "stage": "deploy", "event": "finished", "run": "ab4fccdd", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T20:10:30Z"}},
    {"at": "2026-09-14T19:10:37.745477Z", "stage": "deploy", "event": "finished", "run": "988befa5", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T20:10:37Z"}},
    {"at": "2026-09-15T05:08:01.309654Z", "stage": "verify", "event": "started", "run": "5155a0e4", "detail": {"lease_minutes": 150, "rung": "1h"}},
  ] } },
  { "id": "ORD-2811", "column": "verifying", "body": {"item": "ORD-2811", "stage": "verify", "created_at": "2026-09-14T16:00:03.314469Z", "updated_at": "2026-09-15T05:06:50.068864Z", "stages": {"deploy": {"at": "2026-09-14T18:50:24.869635Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T16:33:13.792959Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T17:08:56.294332Z", "last_event": "gate"}, "review": {"at": "2026-09-14T17:08:57.466439Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T05:06:50.068864Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-14T16:00:03.314469Z", "stage": "implement", "event": "claimed", "run": "8f235c29", "detail": {}},
    {"at": "2026-09-14T16:33:13.792959Z", "stage": "implement", "event": "pr-opened", "run": "8f235c29", "detail": {}},
    {"at": "2026-09-14T16:43:04.192696Z", "stage": "review", "event": "claimed", "run": "8616c1d3", "detail": {}},
    {"at": "2026-09-14T17:08:56.294332Z", "stage": "merge", "event": "gate", "run": "8616c1d3", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T17:08:57.466439Z", "stage": "review", "event": "finished", "run": "8616c1d3", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T18:16:05.003890Z", "stage": "deploy", "event": "claimed", "run": "7e6bbacb", "detail": {}},
    {"at": "2026-09-14T18:46:20.293784Z", "stage": "deploy", "event": "finished", "run": "474142d6", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T19:46:20Z"}},
    {"at": "2026-09-14T18:46:22.075370Z", "stage": "deploy", "event": "finished", "run": "9532db33", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T19:46:22Z"}},
    {"at": "2026-09-14T18:50:24.869635Z", "stage": "deploy", "event": "finished", "run": "a42ac0f3", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T19:50:24Z"}},
    {"at": "2026-09-15T04:53:01.553654Z", "stage": "verify", "event": "started", "run": "63969aae", "detail": {"lease_minutes": 150, "rung": "1h"}},
    {"at": "2026-09-15T05:06:50.068864Z", "stage": "verify", "event": "inconclusive", "run": "63969aae", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-15T00:50:24Z"}},
  ] } },
  { "id": "ORD-2805", "column": "verifying", "body": {"item": "ORD-2805", "stage": "verify", "created_at": "2026-09-14T13:15:03.603212Z", "updated_at": "2026-09-15T05:06:11.161040Z", "stages": {"deploy": {"at": "2026-09-14T18:54:45.236228Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T14:27:03.326210Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T15:09:44.702609Z", "last_event": "gate"}, "review": {"at": "2026-09-14T15:09:46.102062Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T05:06:11.161040Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-14T13:15:03.603212Z", "stage": "implement", "event": "claimed", "run": "a2c6f0a9", "detail": {}},
    {"at": "2026-09-14T14:27:03.326210Z", "stage": "implement", "event": "pr-opened", "run": "a2c6f0a9", "detail": {}},
    {"at": "2026-09-14T14:33:05.446249Z", "stage": "review", "event": "claimed", "run": "3acfb9cb", "detail": {}},
    {"at": "2026-09-14T15:09:44.702609Z", "stage": "merge", "event": "gate", "run": "3acfb9cb", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T15:09:46.102062Z", "stage": "review", "event": "finished", "run": "3acfb9cb", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T18:21:04.347041Z", "stage": "deploy", "event": "claimed", "run": "fae70e19", "detail": {}},
    {"at": "2026-09-14T18:54:45.236228Z", "stage": "deploy", "event": "finished", "run": "1e1d9a00", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T19:54:45Z"}},
    {"at": "2026-09-15T04:58:00.875646Z", "stage": "verify", "event": "started", "run": "ae3f35da", "detail": {"lease_minutes": 150, "rung": "1h"}},
    {"at": "2026-09-15T05:06:11.161040Z", "stage": "verify", "event": "inconclusive", "run": "ae3f35da", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-15T00:54:45Z"}},
  ] } },
  { "id": "ORD-2797", "column": "verifying", "body": {"item": "ORD-2797", "stage": "verify", "created_at": "2026-09-14T10:15:03.343111Z", "updated_at": "2026-09-15T04:53:20.286220Z", "stages": {"deploy": {"at": "2026-09-14T18:28:35.428027Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T17:19:41.035731Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T17:48:24.022657Z", "last_event": "gate"}, "review": {"at": "2026-09-14T17:48:26.934771Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T04:53:20.286220Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-14T10:15:03.343111Z", "stage": "implement", "event": "claimed", "run": "dc86d601", "detail": {}},
    {"at": "2026-09-14T11:02:05.543565Z", "stage": "implement", "event": "pr-opened", "run": "dc86d601", "detail": {}},
    {"at": "2026-09-14T11:03:02.507453Z", "stage": "review", "event": "claimed", "run": "4651992f", "detail": {}},
    {"at": "2026-09-14T12:19:48.580991Z", "stage": "review", "event": "claimed", "run": "cabe2a01", "detail": {}},
    {"at": "2026-09-14T12:43:56.358266Z", "stage": "review", "event": "sent-back", "run": "cabe2a01", "detail": {}},
    {"at": "2026-09-14T13:00:03.550479Z", "stage": "implement", "event": "claimed", "run": "19d755d8", "detail": {}},
    {"at": "2026-09-14T16:30:03.207001Z", "stage": "implement", "event": "claimed", "run": "2e083958", "detail": {}},
    {"at": "2026-09-14T17:19:41.035731Z", "stage": "implement", "event": "pr-opened", "run": "2e083958", "detail": {}},
    {"at": "2026-09-14T17:23:05.179169Z", "stage": "review", "event": "claimed", "run": "343b8454", "detail": {}},
    {"at": "2026-09-14T17:48:24.022657Z", "stage": "merge", "event": "gate", "run": "343b8454", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T17:48:25.575326Z", "stage": "review", "event": "follow-ups", "run": "343b8454", "detail": {}},
    {"at": "2026-09-14T17:48:26.934771Z", "stage": "review", "event": "finished", "run": "343b8454", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T18:26:03.983203Z", "stage": "deploy", "event": "claimed", "run": "7dfa1eb7", "detail": {}},
    {"at": "2026-09-14T18:28:35.428027Z", "stage": "deploy", "event": "finished", "run": "7dfa1eb7", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T19:28:35Z"}},
    {"at": "2026-09-15T04:48:00.771949Z", "stage": "verify", "event": "started", "run": "0e3be51e", "detail": {"lease_minutes": 150, "rung": "1h"}},
    {"at": "2026-09-15T04:53:20.286220Z", "stage": "verify", "event": "inconclusive", "run": "0e3be51e", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-15T00:28:35Z"}},
  ] } },
  { "id": "ORD-2804", "column": "verifying", "body": {"item": "ORD-2804", "stage": "verify", "created_at": "2026-09-14T12:00:03.156762Z", "updated_at": "2026-09-15T04:49:58.894865Z", "stages": {"deploy": {"at": "2026-09-14T16:04:38.120529Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T12:51:49.959984Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T14:54:11.813849Z", "last_event": "gate"}, "review": {"at": "2026-09-14T14:54:13.248624Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T04:49:58.894865Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-14T12:00:03.156762Z", "stage": "implement", "event": "claimed", "run": "ce90c07c", "detail": {}},
    {"at": "2026-09-14T12:51:49.959984Z", "stage": "implement", "event": "pr-opened", "run": "ce90c07c", "detail": {}},
    {"at": "2026-09-14T13:13:04.052859Z", "stage": "review", "event": "claimed", "run": "05c0e5c4", "detail": {}},
    {"at": "2026-09-14T14:54:11.813849Z", "stage": "merge", "event": "gate", "run": "05c0e5c4", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T14:54:13.248624Z", "stage": "review", "event": "finished", "run": "05c0e5c4", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T15:31:03.251274Z", "stage": "deploy", "event": "claimed", "run": "fb7d8f7c", "detail": {}},
    {"at": "2026-09-14T16:04:38.120529Z", "stage": "deploy", "event": "finished", "run": "09e2782f", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T17:04:38Z"}},
    {"at": "2026-09-15T04:43:00.833679Z", "stage": "verify", "event": "started", "run": "b6ad0578", "detail": {"lease_minutes": 150, "rung": "1h"}},
    {"at": "2026-09-15T04:49:58.894865Z", "stage": "verify", "event": "inconclusive", "run": "b6ad0578", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-14T22:04:38Z"}},
  ] } },
  { "id": "ORD-2806", "column": "verifying", "body": {"item": "ORD-2806", "stage": "verify", "created_at": "2026-09-14T13:30:03.279966Z", "updated_at": "2026-09-15T04:45:16.262191Z", "stages": {"deploy": {"at": "2026-09-14T15:59:57.515604Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T13:55:59.904499Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T14:23:48.810341Z", "last_event": "gate"}, "review": {"at": "2026-09-14T14:23:51.789094Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T04:45:16.262191Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-14T13:30:03.279966Z", "stage": "implement", "event": "claimed", "run": "c44c830d", "detail": {}},
    {"at": "2026-09-14T13:55:59.904499Z", "stage": "implement", "event": "pr-opened", "run": "c44c830d", "detail": {}},
    {"at": "2026-09-14T14:03:05.473525Z", "stage": "review", "event": "claimed", "run": "4c4a001e", "detail": {}},
    {"at": "2026-09-14T14:23:48.810341Z", "stage": "merge", "event": "gate", "run": "4c4a001e", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T14:23:50.336418Z", "stage": "review", "event": "follow-ups", "run": "4c4a001e", "detail": {}},
    {"at": "2026-09-14T14:23:51.789094Z", "stage": "review", "event": "finished", "run": "4c4a001e", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T15:26:03.975163Z", "stage": "deploy", "event": "claimed", "run": "1df32df7", "detail": {}},
    {"at": "2026-09-14T15:59:57.515604Z", "stage": "deploy", "event": "finished", "run": "f622500b", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T16:59:57Z"}},
    {"at": "2026-09-15T04:38:01.077009Z", "stage": "verify", "event": "started", "run": "dd1906b5", "detail": {"lease_minutes": 150, "rung": "1h"}},
    {"at": "2026-09-15T04:45:16.262191Z", "stage": "verify", "event": "inconclusive", "run": "dd1906b5", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-14T21:59:57Z"}},
  ] } },
  { "id": "ORD-2759", "column": "verifying", "body": {"item": "ORD-2759", "stage": "verify", "created_at": "2026-09-12T20:01:04.159189141+00:00", "updated_at": "2026-09-15T04:38:09.797096Z", "stages": {"deploy": {"at": "2026-09-12T20:33:38.248511704+00:00", "last_event": "finished"}, "verify": {"at": "2026-09-15T04:38:09.797096Z", "last_event": "inconclusive"}}, "ticket": "ORD-2759", "history": [
    {"at": "2026-09-12T20:01:04.159189141+00:00", "stage": "deploy", "event": "claimed", "run": "74f81e83", "detail": {}},
    {"at": "2026-09-12T20:33:38.248511704+00:00", "stage": "deploy", "event": "finished", "run": "35d159c1", "detail": {"label": "stage-deployed", "due_at": "2026-09-12T21:33:38Z"}},
    {"at": "2026-09-12T22:28:57.173525Z", "stage": "verify", "event": "inconclusive", "run": "a7d93b57", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T02:33:38Z"}},
    {"at": "2026-09-13T08:35:34.568955Z", "stage": "verify", "event": "rung", "run": "ea9f714e", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "not-fixed", "due_at": "2026-09-13T20:33:38Z"}},
    {"at": "2026-09-13T20:38:01.009766Z", "stage": "verify", "event": "started", "run": "9338ef45", "detail": {"rung": "24h"}},
    {"at": "2026-09-13T20:47:56.480786Z", "stage": "verify", "event": "rung", "run": "9338ef45", "detail": {"final": "false", "next_rung": "none", "rung": "24h", "state": "not-fixed", "due_at": "2026-09-15T20:33:38Z"}},
    {"at": "2026-09-15T04:18:01.419291Z", "stage": "verify", "event": "started", "run": "f8c1587b", "detail": {"lease_minutes": 150, "rung": "24h"}},
    {"at": "2026-09-15T04:38:09.797096Z", "stage": "verify", "event": "inconclusive", "run": "f8c1587b", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-15T20:33:38Z"}},
  ] } },
  { "id": "ORD-2786", "column": "verified", "body": {"item": "ORD-2786", "stage": "verify", "created_at": "2026-09-12T19:10:02.634029109+00:00", "updated_at": "2026-09-15T04:36:57.544095Z", "stages": {"deploy": {"at": "2026-09-13T04:53:31.503577Z", "last_event": "finished"}, "implement": {"at": "2026-09-12T19:29:57.609633187+00:00", "last_event": "pr-opened"}, "merge": {"at": "2026-09-13T00:09:05.772418Z", "last_event": "gate"}, "review": {"at": "2026-09-13T00:09:07.108689Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T04:36:57.544095Z", "last_event": "verified"}}, "ticket": "ORD-2786", "history": [
    {"at": "2026-09-12T19:10:02.634029109+00:00", "stage": "implement", "event": "claimed", "run": "667f36b9", "detail": {}},
    {"at": "2026-09-12T19:29:57.609633187+00:00", "stage": "implement", "event": "pr-opened", "run": "667f36b9", "detail": {}},
    {"at": "2026-09-12T19:37:06.957011743+00:00", "stage": "review", "event": "claimed", "run": "c264cc5e", "detail": {}},
    {"at": "2026-09-12T19:56:42.329362175+00:00", "stage": "merge", "event": "gate", "run": "c264cc5e", "detail": {"outcome": "human"}},
    {"at": "2026-09-12T19:56:43.887209202+00:00", "stage": "review", "event": "follow-ups", "run": "c264cc5e", "detail": {}},
    {"at": "2026-09-12T19:56:45.209338617+00:00", "stage": "review", "event": "finished", "run": "c264cc5e", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-12T23:52:05.455633Z", "stage": "review", "event": "claimed", "run": "76fc6ef2", "detail": {}},
    {"at": "2026-09-13T00:09:05.772418Z", "stage": "merge", "event": "gate", "run": "76fc6ef2", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T00:09:07.108689Z", "stage": "review", "event": "finished", "run": "76fc6ef2", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T04:46:03.375300Z", "stage": "deploy", "event": "claimed", "run": "62a48a75", "detail": {}},
    {"at": "2026-09-13T04:53:31.503577Z", "stage": "deploy", "event": "finished", "run": "62a48a75", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T05:53:31Z"}},
    {"at": "2026-09-13T09:22:17.571585Z", "stage": "verify", "event": "rung", "run": "838fdb04", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "pass", "due_at": "2026-09-13T10:53:31Z"}},
    {"at": "2026-09-13T12:53:53.767341Z", "stage": "verify", "event": "verified", "run": "4587bdd6", "detail": {"rung": "6h", "state": "pass"}},
    {"at": "2026-09-13T12:59:41.454694Z", "stage": "verify", "event": "rung", "run": "d27673e6", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "pass", "due_at": "2026-09-14T04:53:31Z"}},
    {"at": "2026-09-15T04:36:57.544095Z", "stage": "verify", "event": "verified", "run": "6b043b6d", "detail": {"reconciled": "true", "rung": "6h", "state": "pass"}},
  ] } },
  { "id": "ORD-2802", "column": "verifying", "body": {"item": "ORD-2802", "stage": "verify", "created_at": "2026-09-14T11:30:03.270593Z", "updated_at": "2026-09-15T04:35:36.584821Z", "stages": {"deploy": {"at": "2026-09-14T15:50:07.628265Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T12:01:00.655952Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T13:08:30.425524Z", "last_event": "gate"}, "review": {"at": "2026-09-14T13:08:31.881825Z", "last_event": "finished"}, "verify": {"at": "2026-09-15T04:35:36.584821Z", "last_event": "rung"}}, "history": [
    {"at": "2026-09-14T11:30:03.270593Z", "stage": "implement", "event": "claimed", "run": "cfa71d89", "detail": {}},
    {"at": "2026-09-14T12:01:00.655952Z", "stage": "implement", "event": "pr-opened", "run": "cfa71d89", "detail": {}},
    {"at": "2026-09-14T12:03:04.097918Z", "stage": "review", "event": "claimed", "run": "778cf0f0", "detail": {}},
    {"at": "2026-09-14T12:35:53.263391Z", "stage": "review", "event": "finished", "run": "778cf0f0", "detail": {"verdict": "stage-pr-open"}},
    {"at": "2026-09-14T12:43:03.138472Z", "stage": "review", "event": "claimed", "run": "98119bc7", "detail": {}},
    {"at": "2026-09-14T13:08:30.425524Z", "stage": "merge", "event": "gate", "run": "98119bc7", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T13:08:31.881825Z", "stage": "review", "event": "finished", "run": "98119bc7", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T15:16:04.323003Z", "stage": "deploy", "event": "claimed", "run": "93004b9c", "detail": {}},
    {"at": "2026-09-14T15:48:06.855425Z", "stage": "deploy", "event": "finished", "run": "c01d912c", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T16:48:06Z"}},
    {"at": "2026-09-14T15:48:40.668794Z", "stage": "deploy", "event": "finished", "run": "8e2c1b74", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T16:48:40Z"}},
    {"at": "2026-09-14T15:50:07.628265Z", "stage": "deploy", "event": "finished", "run": "a9c34f87", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T16:50:07Z"}},
    {"at": "2026-09-15T04:23:01.592101Z", "stage": "verify", "event": "started", "run": "259c1550", "detail": {"lease_minutes": 150, "rung": "1h"}},
    {"at": "2026-09-15T04:35:36.584821Z", "stage": "verify", "event": "rung", "run": "259c1550", "detail": {"next_rung": "none", "rung": "1h", "state": "pass", "due_at": "2026-09-14T21:50:07Z"}},
  ] } },
  { "id": "ORD-2817", "column": "reviewed", "body": {"item": "ORD-2817", "stage": "review", "created_at": "2026-09-14T20:45:03.133716Z", "updated_at": "2026-09-14T22:44:28.771803Z", "stages": {"implement": {"at": "2026-09-14T21:48:40.028656Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T22:44:27.617529Z", "last_event": "gate"}, "review": {"at": "2026-09-14T22:44:28.771803Z", "last_event": "finished"}}, "history": [
    {"at": "2026-09-14T20:45:03.133716Z", "stage": "implement", "event": "claimed", "run": "91511dc1", "detail": {}},
    {"at": "2026-09-14T21:48:40.028656Z", "stage": "implement", "event": "pr-opened", "run": "91511dc1", "detail": {}},
    {"at": "2026-09-14T21:53:04.298003Z", "stage": "review", "event": "claimed", "run": "0d74281e", "detail": {}},
    {"at": "2026-09-14T22:44:27.617529Z", "stage": "merge", "event": "gate", "run": "0d74281e", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T22:44:28.771803Z", "stage": "review", "event": "finished", "run": "0d74281e", "detail": {"verdict": "stage-reviewed"}},
  ] } },
  { "id": "ORD-2816", "column": "reviewed", "body": {"item": "ORD-2816", "stage": "review", "created_at": "2026-09-14T19:45:02.568098Z", "updated_at": "2026-09-14T22:26:11.459181Z", "stages": {"implement": {"at": "2026-09-14T20:23:44.533521Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T22:26:10.157632Z", "last_event": "gate"}, "review": {"at": "2026-09-14T22:26:11.459181Z", "last_event": "finished"}}, "history": [
    {"at": "2026-09-14T19:45:02.568098Z", "stage": "implement", "event": "claimed", "run": "6112ba55", "detail": {}},
    {"at": "2026-09-14T20:23:44.533521Z", "stage": "implement", "event": "pr-opened", "run": "6112ba55", "detail": {}},
    {"at": "2026-09-14T20:43:04.411992Z", "stage": "review", "event": "claimed", "run": "eab7062a", "detail": {}},
    {"at": "2026-09-14T22:26:10.157632Z", "stage": "merge", "event": "gate", "run": "eab7062a", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T22:26:11.459181Z", "stage": "review", "event": "finished", "run": "eab7062a", "detail": {"verdict": "stage-reviewed"}},
  ] } },
  { "id": "ORD-2796", "column": "deployed", "body": {"item": "ORD-2796", "stage": "deploy", "created_at": "2026-09-14T10:01:07.875212Z", "updated_at": "2026-09-14T19:13:43.822235Z", "stages": {"deploy": {"at": "2026-09-14T19:13:43.822235Z", "last_event": "finished"}, "implement": {"at": "2026-09-14T10:28:11.789916Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-14T11:05:08.574433Z", "last_event": "gate"}, "review": {"at": "2026-09-14T11:05:09.984253Z", "last_event": "finished"}}, "history": [
    {"at": "2026-09-14T10:01:07.875212Z", "stage": "implement", "event": "claimed", "run": "472fee70", "detail": {}},
    {"at": "2026-09-14T10:28:11.789916Z", "stage": "implement", "event": "pr-opened", "run": "472fee70", "detail": {}},
    {"at": "2026-09-14T10:33:07.057149Z", "stage": "review", "event": "claimed", "run": "0ea9b605", "detail": {}},
    {"at": "2026-09-14T11:05:08.574433Z", "stage": "merge", "event": "gate", "run": "0ea9b605", "detail": {"outcome": "human"}},
    {"at": "2026-09-14T11:05:09.984253Z", "stage": "review", "event": "finished", "run": "0ea9b605", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-14T19:06:02.369489Z", "stage": "deploy", "event": "claimed", "run": "81dd6d86", "detail": {}},
    {"at": "2026-09-14T19:13:43.822235Z", "stage": "deploy", "event": "finished", "run": "81dd6d86", "detail": {"label": "stage-deployed", "due_at": "2026-09-14T20:13:43Z"}},
  ] } },
  { "id": "ORD-2812", "column": "not-code", "body": {"item": "ORD-2812", "stage": "implement", "created_at": "2026-09-14T16:45:03.574330Z", "updated_at": "2026-09-14T17:14:55.407896Z", "stages": {"implement": {"at": "2026-09-14T17:14:55.407896Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-14T16:45:03.574330Z", "stage": "implement", "event": "claimed", "run": "47565f89", "detail": {}},
    {"at": "2026-09-14T17:14:55.407896Z", "stage": "implement", "event": "released", "run": "47565f89", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2810", "column": "not-code", "body": {"item": "ORD-2810", "stage": "implement", "created_at": "2026-09-14T15:45:03.442774Z", "updated_at": "2026-09-14T16:20:36.358639Z", "stages": {"implement": {"at": "2026-09-14T16:20:36.358639Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-14T15:45:03.442774Z", "stage": "implement", "event": "claimed", "run": "4cf6406e", "detail": {}},
    {"at": "2026-09-14T16:20:36.358639Z", "stage": "implement", "event": "released", "run": "4cf6406e", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2809", "column": "not-code", "body": {"item": "ORD-2809", "stage": "implement", "created_at": "2026-09-14T15:30:03.569748Z", "updated_at": "2026-09-14T15:41:29.010734Z", "stages": {"implement": {"at": "2026-09-14T15:41:29.010734Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-14T15:30:03.569748Z", "stage": "implement", "event": "claimed", "run": "d3a025cf", "detail": {}},
    {"at": "2026-09-14T15:41:29.010734Z", "stage": "implement", "event": "released", "run": "d3a025cf", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2808", "column": "not-code", "body": {"item": "ORD-2808", "stage": "implement", "created_at": "2026-09-14T14:30:03.412391Z", "updated_at": "2026-09-14T15:15:01.477069Z", "stages": {"implement": {"at": "2026-09-14T15:15:01.477069Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-14T14:30:03.412391Z", "stage": "implement", "event": "claimed", "run": "b9bce83a", "detail": {}},
    {"at": "2026-09-14T14:49:26.564138Z", "stage": "implement", "event": "released", "run": "b9bce83a", "detail": {"label": null}},
    {"at": "2026-09-14T15:00:03.563387Z", "stage": "implement", "event": "claimed", "run": "3f4d0610", "detail": {}},
    {"at": "2026-09-14T15:15:01.477069Z", "stage": "implement", "event": "released", "run": "3f4d0610", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2801", "column": "not-code", "body": {"item": "ORD-2801", "stage": "implement", "created_at": "2026-09-14T11:15:03.410703Z", "updated_at": "2026-09-14T11:25:12.172048Z", "stages": {"implement": {"at": "2026-09-14T11:25:12.172048Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-14T11:15:03.410703Z", "stage": "implement", "event": "claimed", "run": "d6b07051", "detail": {}},
    {"at": "2026-09-14T11:25:12.172048Z", "stage": "implement", "event": "released", "run": "d6b07051", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2800", "column": "not-code", "body": {"item": "ORD-2800", "stage": "implement", "created_at": "2026-09-14T11:00:03.172684Z", "updated_at": "2026-09-14T11:08:15.204301Z", "stages": {"implement": {"at": "2026-09-14T11:08:15.204301Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-14T11:00:03.172684Z", "stage": "implement", "event": "claimed", "run": "dad60acb", "detail": {}},
    {"at": "2026-09-14T11:08:15.204301Z", "stage": "implement", "event": "released", "run": "dad60acb", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2799", "column": "not-code", "body": {"item": "ORD-2799", "stage": "implement", "created_at": "2026-09-14T10:45:03.934160Z", "updated_at": "2026-09-14T10:52:45.157410Z", "stages": {"implement": {"at": "2026-09-14T10:52:45.157410Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-14T10:45:03.934160Z", "stage": "implement", "event": "claimed", "run": "b8ad2d00", "detail": {}},
    {"at": "2026-09-14T10:52:45.157410Z", "stage": "implement", "event": "released", "run": "b8ad2d00", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2798", "column": "not-code", "body": {"item": "ORD-2798", "stage": "implement", "created_at": "2026-09-14T10:30:03.632043Z", "updated_at": "2026-09-14T10:37:56.280956Z", "stages": {"implement": {"at": "2026-09-14T10:37:56.280956Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-14T10:30:03.632043Z", "stage": "implement", "event": "claimed", "run": "d2cd44d1", "detail": {}},
    {"at": "2026-09-14T10:37:56.280956Z", "stage": "implement", "event": "released", "run": "d2cd44d1", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2757", "column": "verifying", "body": {"item": "ORD-2757", "stage": "verify", "created_at": "2026-09-13T00:07:06.499799Z", "updated_at": "2026-09-14T10:23:54.845394Z", "stages": {"deploy": {"at": "2026-09-13T10:07:45.088311Z", "last_event": "finished"}, "merge": {"at": "2026-09-13T09:54:00.547946Z", "last_event": "gate"}, "review": {"at": "2026-09-13T09:54:02.429489Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T10:23:54.845394Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-13T00:07:06.499799Z", "stage": "review", "event": "claimed", "run": "67cef34a", "detail": {}},
    {"at": "2026-09-13T01:39:03.898328Z", "stage": "merge", "event": "gate", "run": "67cef34a", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T01:39:07.902013Z", "stage": "review", "event": "follow-ups", "run": "67cef34a", "detail": {}},
    {"at": "2026-09-13T01:39:09.263565Z", "stage": "review", "event": "finished", "run": "67cef34a", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T09:23:02.722526Z", "stage": "review", "event": "claimed", "run": "010994c5", "detail": {}},
    {"at": "2026-09-13T09:54:00.547946Z", "stage": "merge", "event": "gate", "run": "010994c5", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T09:54:02.429489Z", "stage": "review", "event": "finished", "run": "010994c5", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T10:00:54.956349Z", "stage": "deploy", "event": "claimed", "run": "6803dd33", "detail": {}},
    {"at": "2026-09-13T10:07:45.088311Z", "stage": "deploy", "event": "finished", "run": "6803dd33", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T11:07:45Z"}},
    {"at": "2026-09-13T16:23:03.026506Z", "stage": "verify", "event": "started", "run": "9608c79f", "detail": {"rung": "1h"}},
    {"at": "2026-09-13T16:31:28.784234Z", "stage": "verify", "event": "inconclusive", "run": "9608c79f", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T16:07:45Z"}},
    {"at": "2026-09-13T17:33:05.633141Z", "stage": "verify", "event": "started", "run": "610637ba", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:40:51.740348Z", "stage": "verify", "event": "inconclusive", "run": "610637ba", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T10:07:45Z"}},
    {"at": "2026-09-14T10:18:02.720793Z", "stage": "verify", "event": "started", "run": "6812a8d2", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T10:23:54.845394Z", "stage": "verify", "event": "inconclusive", "run": "6812a8d2", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T10:07:45Z"}},
  ] } },
  { "id": "ORD-2791", "column": "verifying", "body": {"item": "ORD-2791", "stage": "verify", "created_at": "2026-09-13T01:50:02.438463Z", "updated_at": "2026-09-14T10:18:29.962180Z", "stages": {"deploy": {"at": "2026-09-13T08:24:50.043677Z", "last_event": "finished"}, "implement": {"at": "2026-09-13T03:04:14.474307Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-13T06:11:52.392819Z", "last_event": "gate"}, "review": {"at": "2026-09-13T06:11:53.548523Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T10:18:29.962180Z", "last_event": "rung"}}, "history": [
    {"at": "2026-09-13T01:50:02.438463Z", "stage": "implement", "event": "claimed", "run": "02ae9b71", "detail": {}},
    {"at": "2026-09-13T03:04:14.474307Z", "stage": "implement", "event": "pr-opened", "run": "02ae9b71", "detail": {}},
    {"at": "2026-09-13T05:37:04.091304Z", "stage": "review", "event": "claimed", "run": "2de46820", "detail": {}},
    {"at": "2026-09-13T06:11:52.392819Z", "stage": "merge", "event": "gate", "run": "2de46820", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T06:11:53.548523Z", "stage": "review", "event": "finished", "run": "2de46820", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T08:21:03.253371Z", "stage": "deploy", "event": "claimed", "run": "df41fb1c", "detail": {}},
    {"at": "2026-09-13T08:24:50.043677Z", "stage": "deploy", "event": "finished", "run": "df41fb1c", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T09:24:50Z"}},
    {"at": "2026-09-13T12:37:44.316565Z", "stage": "verify", "event": "inconclusive", "run": "4edb8d19", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T14:24:50Z"}},
    {"at": "2026-09-13T17:23:06.270233Z", "stage": "verify", "event": "started", "run": "ea316ebc", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:27:53.485161Z", "stage": "verify", "event": "rung", "run": "ea316ebc", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "pass", "due_at": "2026-09-14T08:24:50Z"}},
    {"at": "2026-09-14T08:28:05.890311Z", "stage": "verify", "event": "started", "run": "85e70788", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T09:18:04.503781Z", "stage": "verify", "event": "started", "run": "ef644d7d", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T10:13:02.530500Z", "stage": "verify", "event": "started", "run": "fd377849", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T10:18:29.962180Z", "stage": "verify", "event": "rung", "run": "fd377849", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "pass", "due_at": "2026-09-16T08:24:50Z"}},
  ] } },
  { "id": "ORD-2763", "column": "verifying", "body": {"item": "ORD-2763", "stage": "verify", "created_at": "2026-09-13T05:52:05.135951Z", "updated_at": "2026-09-14T10:14:27.023846Z", "stages": {"deploy": {"at": "2026-09-13T10:06:40.925210Z", "last_event": "finished"}, "merge": {"at": "2026-09-13T06:22:26.997627Z", "last_event": "gate"}, "review": {"at": "2026-09-13T06:22:28.707736Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T10:14:27.023846Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-13T05:52:05.135951Z", "stage": "review", "event": "claimed", "run": "8ea99364", "detail": {}},
    {"at": "2026-09-13T06:22:26.997627Z", "stage": "merge", "event": "gate", "run": "8ea99364", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T06:22:28.707736Z", "stage": "review", "event": "finished", "run": "8ea99364", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T08:26:05.342232Z", "stage": "deploy", "event": "claimed", "run": "c3f4b00c", "detail": {}},
    {"at": "2026-09-13T08:29:50.131480Z", "stage": "deploy", "event": "finished", "run": "c3f4b00c", "detail": {"label": "stage-deploy-blocked"}},
    {"at": "2026-09-13T09:15:36.315427Z", "stage": "deploy", "event": "finished", "run": "0fadda2f", "detail": {"label": "stage-deploy-blocked"}},
    {"at": "2026-09-13T10:06:40.925210Z", "stage": "deploy", "event": "finished", "run": "20e6a79f", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T11:06:40Z"}},
    {"at": "2026-09-13T12:01:04.357373Z", "stage": "verify", "event": "inconclusive", "run": "894bb3f4", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T16:06:40Z"}},
    {"at": "2026-09-13T12:11:01.914823Z", "stage": "verify", "event": "rung", "run": "6c550147", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "not-fixed", "due_at": "2026-09-13T16:06:40Z"}},
    {"at": "2026-09-13T16:43:06.656234Z", "stage": "verify", "event": "started", "run": "7e07e63f", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T16:55:10.643315Z", "stage": "verify", "event": "inconclusive", "run": "7e07e63f", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T10:06:40Z"}},
    {"at": "2026-09-14T10:08:02.449338Z", "stage": "verify", "event": "started", "run": "4c142347", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T10:14:27.023846Z", "stage": "verify", "event": "inconclusive", "run": "4c142347", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T10:06:40Z"}},
  ] } },
  { "id": "ORD-2782", "column": "verified", "body": {"item": "ORD-2782", "stage": "verify", "created_at": "2026-09-12T18:16:15.557300852+00:00", "updated_at": "2026-09-14T10:10:44.104791Z", "stages": {"deploy": {"at": "2026-09-13T09:59:23.273795Z", "last_event": "finished"}, "implement": {"at": "2026-09-13T06:12:02.526404Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-13T08:59:33.223490Z", "last_event": "gate"}, "review": {"at": "2026-09-13T08:59:34.485712Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T10:10:44.104791Z", "last_event": "verified"}}, "ticket": "ORD-2782", "history": [
    {"at": "2026-09-12T18:16:15.557300852+00:00", "stage": "implement", "event": "released", "run": "a6266161", "detail": {"label": "stage-unclear"}},
    {"at": "2026-09-13T05:20:02.895785Z", "stage": "implement", "event": "claimed", "run": "dda94591", "detail": {}},
    {"at": "2026-09-13T06:12:02.526404Z", "stage": "implement", "event": "pr-opened", "run": "dda94591", "detail": {}},
    {"at": "2026-09-13T08:33:03.592886Z", "stage": "review", "event": "claimed", "run": "a04c8dfe", "detail": {}},
    {"at": "2026-09-13T08:59:33.223490Z", "stage": "merge", "event": "gate", "run": "a04c8dfe", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T08:59:34.485712Z", "stage": "review", "event": "finished", "run": "a04c8dfe", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T09:31:03.498663Z", "stage": "deploy", "event": "claimed", "run": "f8025b48", "detail": {}},
    {"at": "2026-09-13T09:57:05.959375Z", "stage": "deploy", "event": "finished", "run": "4a359de6", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T10:57:05Z"}},
    {"at": "2026-09-13T09:59:23.273795Z", "stage": "deploy", "event": "finished", "run": "9c62b4f9", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T10:59:23Z"}},
    {"at": "2026-09-13T12:27:53.875473Z", "stage": "verify", "event": "rung", "run": "62e9de59", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T15:59:23Z"}},
    {"at": "2026-09-13T16:28:03.900544Z", "stage": "verify", "event": "started", "run": "2d1bc884", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:18:03.850590Z", "stage": "verify", "event": "started", "run": "76708bb0", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:25:26.740976Z", "stage": "verify", "event": "rung", "run": "76708bb0", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "pass", "due_at": "2026-09-14T09:59:23Z"}},
    {"at": "2026-09-14T10:03:03.020827Z", "stage": "verify", "event": "started", "run": "091990ac", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T10:10:44.104791Z", "stage": "verify", "event": "verified", "run": "091990ac", "detail": {"rung": "24h", "state": "pass"}},
  ] } },
  { "id": "ORD-2784", "column": "verifying", "body": {"item": "ORD-2784", "stage": "verify", "created_at": "2026-09-12T18:17:21.338552139+00:00", "updated_at": "2026-09-14T09:53:32.471597Z", "stages": {"deploy": {"at": "2026-09-13T09:43:26.160991Z", "last_event": "finished"}, "implement": {"at": "2026-09-13T06:01:23.270290Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-13T08:42:25.088620Z", "last_event": "gate"}, "review": {"at": "2026-09-13T08:42:26.538032Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T09:53:32.471597Z", "last_event": "inconclusive"}}, "ticket": "ORD-2784", "history": [
    {"at": "2026-09-12T18:17:21.338552139+00:00", "stage": "implement", "event": "released", "run": "a252a0c8", "detail": {"label": "stage-unclear"}},
    {"at": "2026-09-13T05:25:02.735137Z", "stage": "implement", "event": "claimed", "run": "30148cd9", "detail": {}},
    {"at": "2026-09-13T06:01:23.270290Z", "stage": "implement", "event": "pr-opened", "run": "30148cd9", "detail": {}},
    {"at": "2026-09-13T08:13:05.425175Z", "stage": "review", "event": "claimed", "run": "babe65be", "detail": {}},
    {"at": "2026-09-13T08:42:25.088620Z", "stage": "merge", "event": "gate", "run": "babe65be", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T08:42:26.538032Z", "stage": "review", "event": "finished", "run": "babe65be", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T09:26:03.000757Z", "stage": "deploy", "event": "claimed", "run": "a20a4ada", "detail": {}},
    {"at": "2026-09-13T09:43:26.160991Z", "stage": "deploy", "event": "finished", "run": "a3a560fc", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T10:43:26Z"}},
    {"at": "2026-09-13T12:42:51.751785Z", "stage": "verify", "event": "inconclusive", "run": "0515236d", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T15:43:26Z"}},
    {"at": "2026-09-13T16:58:07.264317Z", "stage": "verify", "event": "started", "run": "f931f9ee", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:02:14.764394Z", "stage": "verify", "event": "inconclusive", "run": "f931f9ee", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T09:43:26Z"}},
    {"at": "2026-09-14T09:48:03.034148Z", "stage": "verify", "event": "started", "run": "7ab561f5", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T09:53:32.471597Z", "stage": "verify", "event": "inconclusive", "run": "7ab561f5", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T09:43:26Z"}},
  ] } },
  { "id": "ORD-2789", "column": "verified", "body": {"item": "ORD-2789", "stage": "verify", "created_at": "2026-09-13T01:40:02.884189Z", "updated_at": "2026-09-14T09:46:49.424804Z", "stages": {"deploy": {"at": "2026-09-13T09:38:30.261539Z", "last_event": "finished"}, "implement": {"at": "2026-09-13T06:14:43.634130Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-13T09:11:00.919529Z", "last_event": "gate"}, "review": {"at": "2026-09-13T09:11:02.127719Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T09:46:49.424804Z", "last_event": "verified"}}, "history": [
    {"at": "2026-09-13T01:40:02.884189Z", "stage": "implement", "event": "claimed", "run": "af67a911", "detail": {}},
    {"at": "2026-09-13T02:41:28.280117Z", "stage": "implement", "event": "released", "run": "af67a911", "detail": {"label": "stage-unclear"}},
    {"at": "2026-09-13T05:30:03.177719Z", "stage": "implement", "event": "claimed", "run": "81ce164d", "detail": {}},
    {"at": "2026-09-13T06:14:43.634130Z", "stage": "implement", "event": "pr-opened", "run": "81ce164d", "detail": {}},
    {"at": "2026-09-13T08:43:04.041975Z", "stage": "review", "event": "claimed", "run": "5a48368c", "detail": {}},
    {"at": "2026-09-13T09:11:00.919529Z", "stage": "merge", "event": "gate", "run": "5a48368c", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T09:11:02.127719Z", "stage": "review", "event": "finished", "run": "5a48368c", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T09:36:02.867132Z", "stage": "deploy", "event": "claimed", "run": "4104a988", "detail": {}},
    {"at": "2026-09-13T09:38:30.261539Z", "stage": "deploy", "event": "finished", "run": "4104a988", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T10:38:30Z"}},
    {"at": "2026-09-13T12:07:42.874923Z", "stage": "verify", "event": "inconclusive", "run": "28955e02", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T15:38:30Z"}},
    {"at": "2026-09-13T17:13:06.373697Z", "stage": "verify", "event": "started", "run": "2fc4464c", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:17:25.512840Z", "stage": "verify", "event": "inconclusive", "run": "2fc4464c", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T09:38:30Z"}},
    {"at": "2026-09-14T09:43:02.893543Z", "stage": "verify", "event": "started", "run": "722bbf94", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T09:46:49.424804Z", "stage": "verify", "event": "verified", "run": "722bbf94", "detail": {"rung": "24h", "state": "pass"}},
  ] } },
  { "id": "ORD-2762", "column": "verifying", "body": {"item": "ORD-2762", "stage": "verify", "created_at": "2026-09-13T00:52:06.249582Z", "updated_at": "2026-09-14T08:44:50.968766Z", "stages": {"deploy": {"at": "2026-09-13T08:33:42.755072Z", "last_event": "finished"}, "merge": {"at": "2026-09-13T05:12:10.091505Z", "last_event": "gate"}, "review": {"at": "2026-09-13T05:12:11.846336Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T08:44:50.968766Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-13T00:52:06.249582Z", "stage": "review", "event": "claimed", "run": "bf208884", "detail": {}},
    {"at": "2026-09-13T05:12:10.091505Z", "stage": "merge", "event": "gate", "run": "bf208884", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T05:12:11.846336Z", "stage": "review", "event": "finished", "run": "bf208884", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T08:06:03.518065Z", "stage": "deploy", "event": "claimed", "run": "d5eefcdd", "detail": {}},
    {"at": "2026-09-13T08:33:42.755072Z", "stage": "deploy", "event": "finished", "run": "3e95cf7d", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T09:33:42Z"}},
    {"at": "2026-09-13T11:40:33.740087Z", "stage": "verify", "event": "inconclusive", "run": "56c7fe8d", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T14:33:42Z"}},
    {"at": "2026-09-13T11:44:55.488771Z", "stage": "verify", "event": "inconclusive", "run": "7f98783d", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T14:33:42Z"}},
    {"at": "2026-09-13T16:38:04.927164Z", "stage": "verify", "event": "started", "run": "eccaa368", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T16:44:07.540225Z", "stage": "verify", "event": "inconclusive", "run": "eccaa368", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T08:33:42Z"}},
    {"at": "2026-09-14T08:38:01.609810Z", "stage": "verify", "event": "started", "run": "fb5ff8bd", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T08:44:50.968766Z", "stage": "verify", "event": "inconclusive", "run": "fb5ff8bd", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T08:33:42Z"}},
  ] } },
  { "id": "ORD-2779", "column": "verifying", "body": {"item": "ORD-2779", "stage": "verify", "created_at": "2026-09-12T18:22:05.343339123+00:00", "updated_at": "2026-09-14T08:31:50.375093Z", "stages": {"deploy": {"at": "2026-09-13T08:20:48.586170Z", "last_event": "finished"}, "merge": {"at": "2026-09-13T05:30:27.511958Z", "last_event": "gate"}, "review": {"at": "2026-09-13T05:30:28.702527Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T08:31:50.375093Z", "last_event": "inconclusive"}}, "ticket": "ORD-2779", "history": [
    {"at": "2026-09-12T18:22:05.343339123+00:00", "stage": "review", "event": "claimed", "run": "93d61276", "detail": {}},
    {"at": "2026-09-13T02:52:05.880436Z", "stage": "review", "event": "claimed", "run": "ee5cb0e3", "detail": {}},
    {"at": "2026-09-13T05:30:27.511958Z", "stage": "merge", "event": "gate", "run": "ee5cb0e3", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T05:30:28.702527Z", "stage": "review", "event": "finished", "run": "ee5cb0e3", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T08:11:02.988616Z", "stage": "deploy", "event": "claimed", "run": "ba210051", "detail": {}},
    {"at": "2026-09-13T08:20:48.586170Z", "stage": "deploy", "event": "finished", "run": "ba210051", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T09:20:48Z"}},
    {"at": "2026-09-13T13:20:20.469976Z", "stage": "verify", "event": "rung", "run": "30692546", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T14:20:48Z"}},
    {"at": "2026-09-13T13:24:59.429337Z", "stage": "verify", "event": "rung", "run": "c9238852", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "not-fixed", "due_at": "2026-09-13T14:20:48Z"}},
    {"at": "2026-09-13T14:34:13.338859Z", "stage": "verify", "event": "rung", "run": "d3c60945", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "not-fixed", "due_at": "2026-09-14T08:20:48Z"}},
    {"at": "2026-09-13T14:42:01.331763Z", "stage": "verify", "event": "rung", "run": "09ec3b80", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "not-fixed", "due_at": "2026-09-14T08:20:48Z"}},
    {"at": "2026-09-14T08:23:01.324668Z", "stage": "verify", "event": "started", "run": "9cd792e4", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T08:31:50.375093Z", "stage": "verify", "event": "inconclusive", "run": "9cd792e4", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T08:20:48Z"}},
  ] } },
  { "id": "ORD-2773", "column": "verifying", "body": {"item": "ORD-2773", "stage": "verify", "created_at": "2026-09-12T23:37:04.522517Z", "updated_at": "2026-09-14T05:33:30.878201Z", "stages": {"deploy": {"at": "2026-09-13T05:23:31.254144Z", "last_event": "finished"}, "merge": {"at": "2026-09-13T00:04:35.832333Z", "last_event": "gate"}, "review": {"at": "2026-09-13T00:04:37.117690Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T05:33:30.878201Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-12T23:37:04.522517Z", "stage": "review", "event": "claimed", "run": "083f62e9", "detail": {}},
    {"at": "2026-09-13T00:04:35.832333Z", "stage": "merge", "event": "gate", "run": "083f62e9", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T00:04:37.117690Z", "stage": "review", "event": "finished", "run": "083f62e9", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T04:41:02.610201Z", "stage": "deploy", "event": "claimed", "run": "31c0574e", "detail": {}},
    {"at": "2026-09-13T05:23:31.254144Z", "stage": "deploy", "event": "finished", "run": "20699ea3", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T06:23:31Z"}},
    {"at": "2026-09-13T13:40:41.611723Z", "stage": "verify", "event": "inconclusive", "run": "c4848a75", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T11:23:31Z"}},
    {"at": "2026-09-13T13:43:55.032401Z", "stage": "verify", "event": "inconclusive", "run": "27f95350", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T11:23:31Z"}},
    {"at": "2026-09-13T16:48:05.508753Z", "stage": "verify", "event": "started", "run": "fb0d4f65", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:38:04.684598Z", "stage": "verify", "event": "started", "run": "352aae9f", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:43:38.907438Z", "stage": "verify", "event": "inconclusive", "run": "352aae9f", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T05:23:31Z"}},
    {"at": "2026-09-14T05:28:07.325970Z", "stage": "verify", "event": "started", "run": "03e23bdf", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T05:33:30.878201Z", "stage": "verify", "event": "inconclusive", "run": "03e23bdf", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T05:23:31Z"}},
  ] } },
  { "id": "ORD-2774", "column": "verifying", "body": {"item": "ORD-2774", "stage": "verify", "created_at": "2026-09-12T21:52:03.747095Z", "updated_at": "2026-09-14T05:30:15.297724Z", "stages": {"deploy": {"at": "2026-09-13T05:18:28.425180Z", "last_event": "finished"}, "merge": {"at": "2026-09-12T23:39:55.689743Z", "last_event": "gate"}, "review": {"at": "2026-09-12T23:39:57.181975Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T05:30:15.297724Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-12T21:52:03.747095Z", "stage": "review", "event": "claimed", "run": "3a771986", "detail": {}},
    {"at": "2026-09-12T23:39:55.689743Z", "stage": "merge", "event": "gate", "run": "3a771986", "detail": {"outcome": "human"}},
    {"at": "2026-09-12T23:39:57.181975Z", "stage": "review", "event": "finished", "run": "3a771986", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T04:36:03.103189Z", "stage": "deploy", "event": "claimed", "run": "73a3ed9c", "detail": {}},
    {"at": "2026-09-13T05:18:28.425180Z", "stage": "deploy", "event": "finished", "run": "d08d449b", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T06:18:28Z"}},
    {"at": "2026-09-13T11:35:25.788679Z", "stage": "verify", "event": "rung", "run": "0be5bd58", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "pass", "due_at": "2026-09-13T11:18:28Z"}},
    {"at": "2026-09-13T14:04:02.525492Z", "stage": "verify", "event": "inconclusive", "run": "0cf23f2f", "detail": {"rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T05:18:28Z"}},
    {"at": "2026-09-13T14:05:58.069158Z", "stage": "verify", "event": "inconclusive", "run": "2e012672", "detail": {"rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T05:18:28Z"}},
    {"at": "2026-09-14T05:23:02.520534Z", "stage": "verify", "event": "started", "run": "c70ab1da", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T05:30:15.297724Z", "stage": "verify", "event": "inconclusive", "run": "c70ab1da", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T05:18:28Z"}},
  ] } },
  { "id": "ORD-2787", "column": "verifying", "body": {"item": "ORD-2787", "stage": "verify", "created_at": "2026-09-12T19:15:02.262601135+00:00", "updated_at": "2026-09-14T05:19:13.221819Z", "stages": {"deploy": {"at": "2026-09-13T05:09:01.021744Z", "last_event": "finished"}, "implement": {"at": "2026-09-12T19:39:04.990137319+00:00", "last_event": "pr-opened"}, "merge": {"at": "2026-09-12T21:51:00.621837Z", "last_event": "gate"}, "review": {"at": "2026-09-12T21:51:01.897016Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T05:19:13.221819Z", "last_event": "inconclusive"}}, "ticket": "ORD-2787", "history": [
    {"at": "2026-09-12T19:15:02.262601135+00:00", "stage": "implement", "event": "claimed", "run": "aede624a", "detail": {}},
    {"at": "2026-09-12T19:39:04.990137319+00:00", "stage": "implement", "event": "pr-opened", "run": "aede624a", "detail": {}},
    {"at": "2026-09-12T19:52:05.480155689+00:00", "stage": "review", "event": "claimed", "run": "c5bb00d9", "detail": {}},
    {"at": "2026-09-12T20:37:02.700302953+00:00", "stage": "review", "event": "claimed", "run": "ac3f6280", "detail": {}},
    {"at": "2026-09-12T21:51:00.621837Z", "stage": "merge", "event": "gate", "run": "ac3f6280", "detail": {"outcome": "human"}},
    {"at": "2026-09-12T21:51:01.897016Z", "stage": "review", "event": "finished", "run": "ac3f6280", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T04:31:03.006885Z", "stage": "deploy", "event": "claimed", "run": "80c96811", "detail": {}},
    {"at": "2026-09-13T05:07:07.955102Z", "stage": "deploy", "event": "finished", "run": "399979e0", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T06:07:07Z"}},
    {"at": "2026-09-13T05:09:01.021744Z", "stage": "deploy", "event": "finished", "run": "2a106e86", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T06:09:01Z"}},
    {"at": "2026-09-13T09:43:49.973623Z", "stage": "verify", "event": "inconclusive", "run": "2b3792bf", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T11:09:01Z"}},
    {"at": "2026-09-13T17:03:06.754500Z", "stage": "verify", "event": "started", "run": "9198a909", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:10:00.133314Z", "stage": "verify", "event": "inconclusive", "run": "9198a909", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T05:09:01Z"}},
    {"at": "2026-09-14T05:13:04.902775Z", "stage": "verify", "event": "started", "run": "9037c7bc", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T05:19:13.221819Z", "stage": "verify", "event": "inconclusive", "run": "9037c7bc", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T05:09:01Z"}},
  ] } },
  { "id": "ORD-2775", "column": "verifying", "body": {"item": "ORD-2775", "stage": "verify", "created_at": "2026-09-13T01:52:07.064148Z", "updated_at": "2026-09-14T05:04:03.098669Z", "stages": {"deploy": {"at": "2026-09-13T04:57:19.327548Z", "last_event": "finished"}, "merge": {"at": "2026-09-13T02:47:32.765507Z", "last_event": "gate"}, "review": {"at": "2026-09-13T02:47:34.075754Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T05:04:03.098669Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-13T01:52:07.064148Z", "stage": "review", "event": "claimed", "run": "2423f52f", "detail": {}},
    {"at": "2026-09-13T02:47:32.765507Z", "stage": "merge", "event": "gate", "run": "2423f52f", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T02:47:34.075754Z", "stage": "review", "event": "finished", "run": "2423f52f", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T04:56:04.903329Z", "stage": "deploy", "event": "claimed", "run": "8d641016", "detail": {}},
    {"at": "2026-09-13T04:57:19.327548Z", "stage": "deploy", "event": "finished", "run": "8d641016", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T05:57:19Z"}},
    {"at": "2026-09-13T09:36:27.493715Z", "stage": "verify", "event": "rung", "run": "d414cf07", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T10:57:19Z"}},
    {"at": "2026-09-13T13:29:07.295390Z", "stage": "verify", "event": "inconclusive", "run": "bf6cdf69", "detail": {"rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T04:57:19Z"}},
    {"at": "2026-09-13T13:33:45.500781Z", "stage": "verify", "event": "inconclusive", "run": "cb0bed83", "detail": {"rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T04:57:19Z"}},
    {"at": "2026-09-14T04:58:02.759150Z", "stage": "verify", "event": "started", "run": "66a2aa4b", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T05:04:03.098669Z", "stage": "verify", "event": "inconclusive", "run": "66a2aa4b", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T04:57:19Z"}},
  ] } },
  { "id": "ORD-2761", "column": "verifying", "body": {"item": "ORD-2761", "stage": "verify", "created_at": "2026-09-13T00:22:06.776813Z", "updated_at": "2026-09-14T04:58:13.771987Z", "stages": {"deploy": {"at": "2026-09-13T04:52:31.919009Z", "last_event": "finished"}, "merge": {"at": "2026-09-13T00:43:45.387525Z", "last_event": "gate"}, "review": {"at": "2026-09-13T00:43:47.248895Z", "last_event": "finished"}, "verify": {"at": "2026-09-14T04:58:13.771987Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-13T00:22:06.776813Z", "stage": "review", "event": "claimed", "run": "4b03eed2", "detail": {}},
    {"at": "2026-09-13T00:43:45.387525Z", "stage": "merge", "event": "gate", "run": "4b03eed2", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T00:43:47.248895Z", "stage": "review", "event": "finished", "run": "4b03eed2", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T04:51:04.151870Z", "stage": "deploy", "event": "claimed", "run": "0ad56f14", "detail": {}},
    {"at": "2026-09-13T04:52:31.919009Z", "stage": "deploy", "event": "finished", "run": "0ad56f14", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T05:52:31Z"}},
    {"at": "2026-09-13T09:00:51.915512Z", "stage": "verify", "event": "rung", "run": "109d9153", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T10:52:31Z"}},
    {"at": "2026-09-13T11:48:35.437598Z", "stage": "verify", "event": "inconclusive", "run": "b2f2367c", "detail": {"rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T04:52:31Z"}},
    {"at": "2026-09-13T11:52:29.625068Z", "stage": "verify", "event": "inconclusive", "run": "ac767023", "detail": {"rung": "6h", "state": "inconclusive", "due_at": "2026-09-14T04:52:31Z"}},
    {"at": "2026-09-14T04:53:02.073670Z", "stage": "verify", "event": "started", "run": "3b10a2c1", "detail": {"rung": "24h"}},
    {"at": "2026-09-14T04:58:13.771987Z", "stage": "verify", "event": "inconclusive", "run": "3b10a2c1", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-16T04:52:31Z"}},
  ] } },
  { "id": "ORD-2778", "column": "verifying", "body": {"item": "ORD-2778", "stage": "verify", "created_at": "2026-09-12T18:09:34.291164100+00:00", "updated_at": "2026-09-13T20:47:40.144532Z", "stages": {"deploy": {"at": "2026-09-12T20:40:05.280367426+00:00", "last_event": "finished"}, "merge": {"at": "2026-09-12T18:09:34.291164100+00:00", "last_event": "gate"}, "review": {"at": "2026-09-12T18:09:37.407482176+00:00", "last_event": "finished"}, "verify": {"at": "2026-09-13T20:47:40.144532Z", "last_event": "inconclusive"}}, "ticket": "ORD-2778", "history": [
    {"at": "2026-09-12T18:09:34.291164100+00:00", "stage": "merge", "event": "gate", "run": "6a3d3627", "detail": {"outcome": "human"}},
    {"at": "2026-09-12T18:09:35.751681087+00:00", "stage": "review", "event": "follow-ups", "run": "6a3d3627", "detail": {}},
    {"at": "2026-09-12T18:09:37.407482176+00:00", "stage": "review", "event": "finished", "run": "6a3d3627", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-12T20:06:08.179103236+00:00", "stage": "deploy", "event": "claimed", "run": "fc6b3716", "detail": {}},
    {"at": "2026-09-12T20:40:05.280367426+00:00", "stage": "deploy", "event": "finished", "run": "b162d73f", "detail": {"label": "stage-deployed", "due_at": "2026-09-12T21:40:05Z"}},
    {"at": "2026-09-12T22:58:52.394418Z", "stage": "verify", "event": "rung", "run": "06f60bb8", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "pass", "due_at": "2026-09-13T02:40:05Z"}},
    {"at": "2026-09-13T02:58:56.637325Z", "stage": "verify", "event": "rung", "run": "76a90850", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-13T20:40:05Z"}},
    {"at": "2026-09-13T20:43:02.253237Z", "stage": "verify", "event": "started", "run": "b5b72245", "detail": {"rung": "24h"}},
    {"at": "2026-09-13T20:47:40.144532Z", "stage": "verify", "event": "inconclusive", "run": "b5b72245", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-15T20:40:05Z"}},
  ] } },
  { "id": "ORD-2785", "column": "verifying", "body": {"item": "ORD-2785", "stage": "verify", "created_at": "2026-09-12T18:10:02.647544859+00:00", "updated_at": "2026-09-13T20:28:40.846658Z", "stages": {"deploy": {"at": "2026-09-12T20:19:56.682356613+00:00", "last_event": "finished"}, "implement": {"at": "2026-09-12T18:42:09.511164120+00:00", "last_event": "pr-opened"}, "merge": {"at": "2026-09-12T19:05:41.896623439+00:00", "last_event": "gate"}, "review": {"at": "2026-09-12T19:05:45.470840020+00:00", "last_event": "finished"}, "verify": {"at": "2026-09-13T20:28:40.846658Z", "last_event": "inconclusive"}}, "ticket": "ORD-2785", "history": [
    {"at": "2026-09-12T18:10:02.647544859+00:00", "stage": "implement", "event": "claimed", "run": "076a3fec", "detail": {}},
    {"at": "2026-09-12T18:42:09.511164120+00:00", "stage": "implement", "event": "pr-opened", "run": "076a3fec", "detail": {}},
    {"at": "2026-09-12T18:52:05.417064927+00:00", "stage": "review", "event": "claimed", "run": "52811886", "detail": {}},
    {"at": "2026-09-12T19:05:41.896623439+00:00", "stage": "merge", "event": "gate", "run": "52811886", "detail": {"outcome": "human"}},
    {"at": "2026-09-12T19:05:44.232528833+00:00", "stage": "review", "event": "follow-ups", "run": "52811886", "detail": {}},
    {"at": "2026-09-12T19:05:45.470840020+00:00", "stage": "review", "event": "finished", "run": "52811886", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-12T20:16:09.122226519+00:00", "stage": "deploy", "event": "claimed", "run": "e229fd5c", "detail": {}},
    {"at": "2026-09-12T20:19:56.682356613+00:00", "stage": "deploy", "event": "finished", "run": "e229fd5c", "detail": {"label": "stage-deployed", "due_at": "2026-09-12T21:19:56Z"}},
    {"at": "2026-09-12T21:59:09.962484Z", "stage": "verify", "event": "inconclusive", "run": "cae1d0c7", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T02:19:56Z"}},
    {"at": "2026-09-13T07:58:50.597067Z", "stage": "verify", "event": "rung", "run": "2a7940a2", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-13T20:19:56Z"}},
    {"at": "2026-09-13T20:23:02.318862Z", "stage": "verify", "event": "started", "run": "ff82424e", "detail": {"rung": "24h"}},
    {"at": "2026-09-13T20:28:40.846658Z", "stage": "verify", "event": "inconclusive", "run": "ff82424e", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-15T20:19:56Z"}},
  ] } },
  { "id": "ORD-2764", "column": "verifying", "body": {"item": "ORD-2764", "stage": "verify", "created_at": "2026-09-12T19:56:06.536437040+00:00", "updated_at": "2026-09-13T20:21:59.808491Z", "stages": {"deploy": {"at": "2026-09-12T20:17:22.076747613+00:00", "last_event": "finished"}, "verify": {"at": "2026-09-13T20:21:59.808491Z", "last_event": "inconclusive"}}, "ticket": "ORD-2764", "history": [
    {"at": "2026-09-12T19:56:06.536437040+00:00", "stage": "deploy", "event": "claimed", "run": "f2be9e81", "detail": {}},
    {"at": "2026-09-12T20:17:22.076747613+00:00", "stage": "deploy", "event": "finished", "run": "f2be9e81", "detail": {"label": "stage-deployed", "due_at": "2026-09-12T21:17:22Z"}},
    {"at": "2026-09-12T21:27:12.684489Z", "stage": "verify", "event": "inconclusive", "run": "bea370e7", "detail": {"rung": "1h", "state": "inconclusive", "due_at": "2026-09-13T02:17:22Z"}},
    {"at": "2026-09-13T05:58:54.711755Z", "stage": "verify", "event": "rung", "run": "c5221df1", "detail": {"final": "false", "next_rung": "none", "rung": "6h", "state": "inconclusive", "due_at": "2026-09-13T20:17:22Z"}},
    {"at": "2026-09-13T20:18:02.033126Z", "stage": "verify", "event": "started", "run": "4dcfdf55", "detail": {"rung": "24h"}},
    {"at": "2026-09-13T20:21:59.808491Z", "stage": "verify", "event": "inconclusive", "run": "4dcfdf55", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive", "due_at": "2026-09-15T20:17:22Z"}},
  ] } },
  { "id": "ORD-2743", "column": "verified", "body": {"item": "ORD-2743", "stage": "verify", "created_at": "2026-09-13T16:13:01.803376Z", "updated_at": "2026-09-13T17:32:52.746826Z", "stages": {"verify": {"at": "2026-09-13T17:32:52.746826Z", "last_event": "verified"}}, "history": [
    {"at": "2026-09-13T16:13:01.803376Z", "stage": "verify", "event": "started", "run": "31f137c7", "detail": {"rung": "1h"}},
    {"at": "2026-09-13T16:14:20.068910Z", "stage": "verify", "event": "rung", "run": "cfa92e2f", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive"}},
    {"at": "2026-09-13T16:18:58.453467Z", "stage": "verify", "event": "inconclusive", "run": "31f137c7", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "inconclusive"}},
    {"at": "2026-09-13T17:28:06.104298Z", "stage": "verify", "event": "started", "run": "162a5291", "detail": {"rung": "6h"}},
    {"at": "2026-09-13T17:32:52.746826Z", "stage": "verify", "event": "verified", "run": "162a5291", "detail": {"rung": "6h", "state": "pass"}},
  ] } },
  { "id": "ORD-2742", "column": "verified", "body": {"item": "ORD-2742", "stage": "verify", "created_at": "2026-09-13T16:18:01.857595Z", "updated_at": "2026-09-13T17:12:21.000177Z", "stages": {"verify": {"at": "2026-09-13T17:12:21.000177Z", "last_event": "verified"}}, "history": [
    {"at": "2026-09-13T16:18:01.857595Z", "stage": "verify", "event": "started", "run": "755923ec", "detail": {"rung": "1h"}},
    {"at": "2026-09-13T17:08:01.940423Z", "stage": "verify", "event": "started", "run": "2c977517", "detail": {"rung": "1h"}},
    {"at": "2026-09-13T17:12:21.000177Z", "stage": "verify", "event": "verified", "run": "2c977517", "detail": {"rung": "1h", "state": "pass"}},
  ] } },
  { "id": "ORD-2758", "column": "verifying", "body": {"item": "ORD-2758", "stage": "verify", "created_at": "2026-09-13T16:33:05.633028Z", "updated_at": "2026-09-13T16:38:00.919031Z", "stages": {"verify": {"at": "2026-09-13T16:38:00.919031Z", "last_event": "inconclusive"}}, "history": [
    {"at": "2026-09-13T16:33:05.633028Z", "stage": "verify", "event": "started", "run": "098e2cc1", "detail": {"rung": "24h"}},
    {"at": "2026-09-13T16:38:00.919031Z", "stage": "verify", "event": "inconclusive", "run": "098e2cc1", "detail": {"final": "false", "next_rung": "72h", "rung": "24h", "state": "inconclusive"}},
  ] } },
  { "id": "ORD-2781", "column": "verified", "body": {"item": "ORD-2781", "stage": "verify", "created_at": "2026-09-12T18:26:03.863463077+00:00", "updated_at": "2026-09-13T16:22:06.955046Z", "stages": {"deploy": {"at": "2026-09-12T20:44:27.250794036+00:00", "last_event": "finished"}, "merge": {"at": "2026-09-12T18:49:06.127710535+00:00", "last_event": "gate"}, "review": {"at": "2026-09-12T18:49:07.317123276+00:00", "last_event": "finished"}, "verify": {"at": "2026-09-13T16:22:06.955046Z", "last_event": "verified"}}, "ticket": "ORD-2781", "history": [
    {"at": "2026-09-12T18:26:03.863463077+00:00", "stage": "review", "event": "claimed", "run": "93d61276", "detail": {}},
    {"at": "2026-09-12T18:49:06.127710535+00:00", "stage": "merge", "event": "gate", "run": "93d61276", "detail": {"outcome": "human"}},
    {"at": "2026-09-12T18:49:07.317123276+00:00", "stage": "review", "event": "finished", "run": "93d61276", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-12T20:11:09.231128150+00:00", "stage": "deploy", "event": "claimed", "run": "5c73016f", "detail": {}},
    {"at": "2026-09-12T20:44:27.250794036+00:00", "stage": "deploy", "event": "finished", "run": "dd62c588", "detail": {"label": "stage-deployed", "due_at": "2026-09-12T21:44:27Z"}},
    {"at": "2026-09-12T23:27:10.820768Z", "stage": "verify", "event": "rung", "run": "7883699b", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "pass", "due_at": "2026-09-13T02:44:27Z"}},
    {"at": "2026-09-13T03:26:40.651506Z", "stage": "verify", "event": "inconclusive", "run": "981cf9f0", "detail": {"rung": "6h", "state": "inconclusive", "due_at": "2026-09-13T20:44:27Z"}},
    {"at": "2026-09-13T16:22:06.955046Z", "stage": "verify", "event": "verified", "run": "533afc0d", "detail": {"rung": "6h", "state": "pass"}},
  ] } },
  { "id": "ORD-2744", "column": "verified", "body": {"item": "ORD-2744", "stage": "verify", "created_at": "2026-09-13T16:06:55.504320Z", "updated_at": "2026-09-13T16:06:55.504320Z", "stages": {"verify": {"at": "2026-09-13T16:06:55.504320Z", "last_event": "verified"}}, "history": [
    {"at": "2026-09-13T16:06:55.504320Z", "stage": "verify", "event": "verified", "run": "7525948d", "detail": {"rung": "1h", "state": "pass"}},
  ] } },
  { "id": "ORD-2746", "column": "verified", "body": {"item": "ORD-2746", "stage": "verify", "created_at": "2026-09-13T15:57:50.775066Z", "updated_at": "2026-09-13T15:57:50.775066Z", "stages": {"verify": {"at": "2026-09-13T15:57:50.775066Z", "last_event": "verified"}}, "history": [
    {"at": "2026-09-13T15:57:50.775066Z", "stage": "verify", "event": "verified", "run": "a31f9f31", "detail": {"rung": "1h", "state": "pass"}},
  ] } },
  { "id": "ORD-2790", "column": "verified", "body": {"item": "ORD-2790", "stage": "verify", "created_at": "2026-09-13T01:45:03.039338Z", "updated_at": "2026-09-13T15:37:10.877233Z", "stages": {"deploy": {"at": "2026-09-13T09:28:08.055467Z", "last_event": "finished"}, "implement": {"at": "2026-09-13T05:47:03.014349Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-13T08:31:58.210655Z", "last_event": "gate"}, "review": {"at": "2026-09-13T08:31:59.369229Z", "last_event": "finished"}, "verify": {"at": "2026-09-13T15:37:10.877233Z", "last_event": "verified"}}, "history": [
    {"at": "2026-09-13T01:45:03.039338Z", "stage": "implement", "event": "claimed", "run": "95611e93", "detail": {}},
    {"at": "2026-09-13T04:50:02.508606Z", "stage": "implement", "event": "claimed", "run": "3779f9fc", "detail": {}},
    {"at": "2026-09-13T05:47:03.014349Z", "stage": "implement", "event": "pr-opened", "run": "3779f9fc", "detail": {}},
    {"at": "2026-09-13T08:03:06.034747Z", "stage": "review", "event": "claimed", "run": "91cc6ffe", "detail": {}},
    {"at": "2026-09-13T08:31:58.210655Z", "stage": "merge", "event": "gate", "run": "91cc6ffe", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T08:31:59.369229Z", "stage": "review", "event": "finished", "run": "91cc6ffe", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T09:21:03.528253Z", "stage": "deploy", "event": "claimed", "run": "1ca7d173", "detail": {}},
    {"at": "2026-09-13T09:28:08.055467Z", "stage": "deploy", "event": "finished", "run": "1ca7d173", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T10:28:08Z"}},
    {"at": "2026-09-13T12:12:39.546394Z", "stage": "verify", "event": "rung", "run": "c13232d0", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "pass", "due_at": "2026-09-13T15:28:08Z"}},
    {"at": "2026-09-13T15:37:10.877233Z", "stage": "verify", "event": "verified", "run": "48237bed", "detail": {"rung": "6h", "state": "pass"}},
  ] } },
  { "id": "ORD-2792", "column": "verified", "body": {"item": "ORD-2792", "stage": "verify", "created_at": "2026-09-13T08:30:02.514821Z", "updated_at": "2026-09-13T13:46:10.533392Z", "stages": {"deploy": {"at": "2026-09-13T11:30:31.611382Z", "last_event": "finished"}, "implement": {"at": "2026-09-13T08:58:06.138301Z", "last_event": "pr-opened"}, "merge": {"at": "2026-09-13T09:31:26.464818Z", "last_event": "gate"}, "review": {"at": "2026-09-13T09:31:27.982979Z", "last_event": "finished"}, "verify": {"at": "2026-09-13T13:46:10.533392Z", "last_event": "verified"}}, "history": [
    {"at": "2026-09-13T08:30:02.514821Z", "stage": "implement", "event": "claimed", "run": "47a1ac83", "detail": {}},
    {"at": "2026-09-13T08:58:06.138301Z", "stage": "implement", "event": "pr-opened", "run": "47a1ac83", "detail": {}},
    {"at": "2026-09-13T09:03:04.556987Z", "stage": "review", "event": "claimed", "run": "dac24d32", "detail": {}},
    {"at": "2026-09-13T09:31:26.464818Z", "stage": "merge", "event": "gate", "run": "dac24d32", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T09:31:27.982979Z", "stage": "review", "event": "finished", "run": "dac24d32", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T09:46:03.736905Z", "stage": "deploy", "event": "claimed", "run": "55d252b7", "detail": {}},
    {"at": "2026-09-13T11:30:31.611382Z", "stage": "deploy", "event": "finished", "run": "fe9c2f64", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T12:30:31Z"}},
    {"at": "2026-09-13T13:46:10.533392Z", "stage": "verify", "event": "verified", "run": "32509592", "detail": {"rung": "1h", "state": "pass"}},
  ] } },
  { "id": "ORD-2788", "column": "verified", "body": {"item": "ORD-2788", "stage": "verify", "created_at": "2026-09-12T20:00:02.249197223+00:00", "updated_at": "2026-09-13T12:32:26.904553Z", "stages": {"deploy": {"at": "2026-09-13T08:29:51.265980Z", "last_event": "finished"}, "implement": {"at": "2026-09-12T20:20:19.275678037+00:00", "last_event": "pr-opened"}, "merge": {"at": "2026-09-13T05:41:23.718550Z", "last_event": "gate"}, "review": {"at": "2026-09-13T05:41:24.809075Z", "last_event": "finished"}, "verify": {"at": "2026-09-13T12:32:26.904553Z", "last_event": "verified"}}, "ticket": "ORD-2788", "history": [
    {"at": "2026-09-12T20:00:02.249197223+00:00", "stage": "implement", "event": "claimed", "run": "c3647b53", "detail": {}},
    {"at": "2026-09-12T20:20:19.275678037+00:00", "stage": "implement", "event": "pr-opened", "run": "c3647b53", "detail": {}},
    {"at": "2026-09-12T20:22:05.345837545+00:00", "stage": "review", "event": "claimed", "run": "1384d8ce", "detail": {}},
    {"at": "2026-09-12T20:29:18.055178821+00:00", "stage": "review", "event": "finished", "run": "1384d8ce", "detail": {"verdict": "stage-pr-open"}},
    {"at": "2026-09-13T05:22:03.524717Z", "stage": "review", "event": "claimed", "run": "3d6c0473", "detail": {}},
    {"at": "2026-09-13T05:41:23.718550Z", "stage": "merge", "event": "gate", "run": "3d6c0473", "detail": {"outcome": "human"}},
    {"at": "2026-09-13T05:41:24.809075Z", "stage": "review", "event": "finished", "run": "3d6c0473", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T08:16:03.769420Z", "stage": "deploy", "event": "claimed", "run": "8e00c593", "detail": {}},
    {"at": "2026-09-13T08:29:51.265980Z", "stage": "deploy", "event": "finished", "run": "8e00c593", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T09:29:51Z"}},
    {"at": "2026-09-13T12:32:26.904553Z", "stage": "verify", "event": "verified", "run": "9dce817b", "detail": {"rung": "1h", "state": "pass"}},
  ] } },
  { "id": "ORD-2765", "column": "verified", "body": {"item": "ORD-2765", "stage": "verify", "created_at": "2026-09-12T20:52:03.644713Z", "updated_at": "2026-09-13T10:00:27.969329Z", "stages": {"deploy": {"at": "2026-09-13T05:14:16.853622Z", "last_event": "finished"}, "merge": {"at": "2026-09-12T23:27:57.095496Z", "last_event": "gate"}, "review": {"at": "2026-09-12T23:28:01.505482Z", "last_event": "finished"}, "verify": {"at": "2026-09-13T10:00:27.969329Z", "last_event": "verified"}}, "history": [
    {"at": "2026-09-12T20:52:03.644713Z", "stage": "review", "event": "claimed", "run": "e6d9a314", "detail": {}},
    {"at": "2026-09-12T23:27:57.095496Z", "stage": "merge", "event": "gate", "run": "e6d9a314", "detail": {"outcome": "human"}},
    {"at": "2026-09-12T23:28:01.505482Z", "stage": "review", "event": "finished", "run": "e6d9a314", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-09-13T04:34:43.135156Z", "stage": "deploy", "event": "claimed", "run": "80c96811", "detail": {}},
    {"at": "2026-09-13T05:14:16.853622Z", "stage": "deploy", "event": "finished", "run": "7002ac9d", "detail": {"label": "stage-deployed", "due_at": "2026-09-13T06:14:16Z"}},
    {"at": "2026-09-13T10:00:21.406679Z", "stage": "verify", "event": "rung", "run": "108ae9cd", "detail": {"final": "false", "next_rung": "none", "rung": "1h", "state": "pass", "due_at": "2026-09-13T11:14:16Z"}},
    {"at": "2026-09-13T10:00:27.969329Z", "stage": "verify", "event": "verified", "run": "dfb62e0c", "detail": {"rung": "1h", "state": "pass"}},
  ] } },
  { "id": "ORD-2793", "column": "not-code", "body": {"item": "ORD-2793", "stage": "implement", "created_at": "2026-09-13T08:45:03.259038Z", "updated_at": "2026-09-13T09:01:58.672624Z", "stages": {"implement": {"at": "2026-09-13T09:01:58.672624Z", "last_event": "released"}}, "history": [
    {"at": "2026-09-13T08:45:03.259038Z", "stage": "implement", "event": "claimed", "run": "66531dd7", "detail": {}},
    {"at": "2026-09-13T09:01:58.672624Z", "stage": "implement", "event": "released", "run": "66531dd7", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2760", "column": "not-code", "body": {"stage": "implement", "created_at": "2026-09-12T18:08:00.684105543+00:00", "updated_at": "2026-09-12T18:20:36.720915075+00:00", "stages": {"implement": {"at": "2026-09-12T18:20:36.720915075+00:00", "last_event": "released"}}, "ticket": "ORD-2760", "history": [
    {"at": "2026-09-12T18:08:00.684105543+00:00", "stage": "implement", "event": "claimed", "run": "0a70853d", "detail": {}},
    {"at": "2026-09-12T18:20:36.720915075+00:00", "stage": "implement", "event": "released", "run": "0a70853d", "detail": {"label": "stage-skipped"}},
  ] } },
  { "id": "ORD-2601", "column": "verified", "body": {"item": "ORD-2601", "stage": "verify", "created_at": "2026-08-19T09:00:03.412210Z", "updated_at": "2026-08-28T10:00:02.118042Z", "stages": {"deploy": {"at": "2026-08-20T14:02:11.920114Z", "last_event": "finished"}, "implement": {"at": "2026-08-19T09:41:27.301558Z", "last_event": "pr-opened"}, "review": {"at": "2026-08-19T10:22:40.550917Z", "last_event": "finished"}, "verify": {"at": "2026-08-28T10:00:02.118042Z", "last_event": "verified"}}, "history": [
    {"at": "2026-08-19T09:00:03.412210Z", "stage": "implement", "event": "claimed", "run": "5a1f0c22", "detail": {}},
    {"at": "2026-08-19T09:41:27.301558Z", "stage": "implement", "event": "pr-opened", "run": "5a1f0c22", "detail": {}},
    {"at": "2026-08-19T09:48:02.771230Z", "stage": "review", "event": "claimed", "run": "9c0e4b17", "detail": {}},
    {"at": "2026-08-19T10:22:40.550917Z", "stage": "review", "event": "finished", "run": "9c0e4b17", "detail": {"verdict": "stage-reviewed"}},
    {"at": "2026-08-20T14:02:11.920114Z", "stage": "deploy", "event": "finished", "run": "e37d52a8", "detail": {"label": "stage-deployed", "due_at": "2026-08-20T15:02:11Z"}},
    {"at": "2026-08-28T10:00:02.118042Z", "stage": "verify", "event": "verified", "run": "41b8e6f3", "detail": {"rung": "168h"}},
  ] } },
  { "id": "ORD-2655", "column": "not-code", "body": {"item": "ORD-2655", "stage": "implement", "created_at": "2026-08-25T09:00:02.604318Z", "updated_at": "2026-08-25T09:10:44.093771Z", "stages": {"implement": {"at": "2026-08-25T09:10:44.093771Z", "last_event": "released"}}, "history": [
    {"at": "2026-08-25T09:00:02.604318Z", "stage": "implement", "event": "claimed", "run": "b62a9d04", "detail": {}},
    {"at": "2026-08-25T09:10:44.093771Z", "stage": "implement", "event": "released", "run": "b62a9d04", "detail": {"label": "stage-skipped"}},
  ] } },
];
