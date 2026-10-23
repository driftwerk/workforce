//! The state for a decision about an inbound request: the scope gate's
//! on-topic question and the routing question read the same thing, so both
//! can be asked in one call.

use crate::text::{collapse, fit, Keep, StateBudget};
use serde_json::{json, Value};

/// The request as its author meant it: pasted logs, traces, code and long
/// quotes collapsed to markers, the question sentences kept first, then
/// the opening and the closing. `None` for a blank message: there is
/// nothing to decide about.
pub fn state(message: &str, budget: StateBudget) -> Option<Value> {
    let message = fit(&collapse(message), budget.get(), Keep::REQUEST);
    (!message.is_empty()).then(|| json!({ "message": message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::ELISION;

    fn built(request: &str, budget: StateBudget) -> String {
        let state = state(request, budget).expect("a state");
        let message = state["message"].as_str().unwrap().to_string();
        assert!(message.chars().count() <= budget.get(), "{message}");
        assert!(!message.is_empty());
        assert_eq!(super::state(request, budget), Some(state), "stable");
        message
    }

    fn default(request: &str) -> String {
        built(request, StateBudget::DEFAULT)
    }

    #[test]
    fn a_short_request_is_the_state_as_written_and_a_blank_one_is_none() {
        assert_eq!(
            state("Why is invoice 42 unpaid?", StateBudget::DEFAULT),
            Some(json!({"message": "Why is invoice 42 unpaid?"}))
        );
        for blank in ["", "   ", "\n\t\n", "\u{3000}"] {
            assert_eq!(state(blank, StateBudget::DEFAULT), None, "{blank:?}");
        }
    }

    #[test]
    fn a_pasted_log_becomes_a_marker_and_the_question_after_it_is_kept() {
        let log: String = (0..400)
            .map(|i| {
                format!(
                    "2026-09-26 10:{:02}:{:02} ERROR db: connection {i} reset by peer\n",
                    i / 60,
                    i % 60
                )
            })
            .collect();
        let request = format!(
            "Our importer died overnight.\n{log}Can you find which deploy broke the connection pool?"
        );
        assert!(request.chars().count() > 20_000);
        assert_eq!(
            default(&request),
            "Our importer died overnight.\n[400 lines of log]\n\
             Can you find which deploy broke the connection pool?"
        );
    }

    const PYTHON: &str = "Traceback (most recent call last):\n  File \"/app/importer.py\", line 88, in <module>\n    main()\n  File \"/app/importer.py\", line 52, in main\n    rows = load(path)\n  File \"/app/loader.py\", line 17, in load\n    raise ValueError(f\"no rows in {path}\")\nValueError: no rows in /data/in.csv";

    const RUST: &str = "thread 'main' panicked at src/import.rs:42:9:\nno rows\nstack backtrace:\n   0: rust_begin_unwind\n             at /rustc/90b35a6/library/std/src/panicking.rs:652:5\n   1: core::panicking::panic_fmt\n             at /rustc/90b35a6/library/core/src/panicking.rs:72:14\n   2: importer::run\n             at ./src/import.rs:42:9\n   3: importer::main\n             at ./src/main.rs:7:5";

    const GO: &str = "panic: runtime error: index out of range [3] with length 3\n\ngoroutine 1 [running]:\nmain.load(...)\n\t/app/load.go:17 +0x1d\nmain.run()\n\t/app/run.go:52 +0x2a\nmain.main()\n\t/app/main.go:9 +0x17\nexit status 2";

    const NODE: &str = "TypeError: Cannot read properties of undefined (reading 'rows')\n    at load (/app/loader.js:17:11)\n    at /app/importer.js:52:18\n    at processTicksAndRejections (node:internal/process/task_queues:95:5)\n    at async main (/app/importer.js:88:3)";

    #[test]
    fn a_trace_in_any_language_collapses_and_the_question_before_or_after_it_stays() {
        for (trace, error) in [
            (PYTHON, "ValueError: no rows in /data/in.csv"),
            (RUST, "no rows"),
            (
                GO,
                "panic: runtime error: index out of range [3] with length 3",
            ),
            (NODE, "TypeError: Cannot read properties of undefined"),
        ] {
            let after = default(&format!("{trace}\nWhich commit broke the importer?"));
            let before = default(&format!("Which commit broke the importer?\n{trace}"));
            for message in [&after, &before] {
                assert!(message.contains("lines of log]"), "{message}");
                assert!(message.contains("Which commit broke the importer?"));
                assert!(message.contains(error), "the error itself stays: {message}");
                assert!(message.lines().count() <= 7, "{message}");
            }
            assert!(after.ends_with("Which commit broke the importer?"));
            assert!(before.starts_with("Which commit broke the importer?"));
        }
    }

    #[test]
    fn a_request_that_is_mostly_a_long_trace_keeps_its_question_at_either_end() {
        let frames: String = (0..600)
            .map(|i| format!("\tat com.example.Import.step{i}(Import.java:{i})\n"))
            .collect();
        let trace = format!("java.lang.IllegalStateException: no rows\n{frames}");
        assert_eq!(
            default(&format!("{trace}How do I fix the importer")),
            "java.lang.IllegalStateException: no rows\n[600 lines of log]\nHow do I fix the importer"
        );
        assert_eq!(
            default(&format!("How do I fix the importer\n{trace}")),
            "How do I fix the importer\njava.lang.IllegalStateException: no rows\n[600 lines of log]"
        );
    }

    #[test]
    fn a_long_request_keeps_every_question_then_its_opening_and_its_closing() {
        let filler = "Some background about how the team works and what happened last week. ";
        let request = format!(
            "Hi! {}Which table holds refunds? {}And who owns the refunds service?\n{}Please reply in the thread.",
            filler.repeat(15),
            filler.repeat(15),
            filler.repeat(15)
        );
        let kept = built(&request, StateBudget::chars(400));
        assert!(kept.contains("Which table holds refunds?"));
        assert!(kept.contains("And who owns the refunds service?"));
        assert!(kept.starts_with("Hi!") && kept.contains(ELISION));
        assert!(kept.ends_with("Please reply in the thread."), "{kept}");
    }

    #[test]
    fn an_ask_without_a_question_mark_survives_at_the_start_or_the_end() {
        let filler = "The deploy went out at noon and the dashboards looked normal. ".repeat(60);
        let ask = "Tell me which service is paging";
        let last = default(&format!("{filler}\n{ask}"));
        assert!(last.ends_with(ask), "{last}");
        let first = default(&format!("{ask}\n{filler}"));
        assert!(first.starts_with(ask), "{first}");
    }

    #[test]
    fn a_very_long_single_line_keeps_the_question_it_ends_or_starts_with() {
        let blob = "eyJhbGciOiJIUzI1NiJ9".repeat(400);
        let unbroken = default(&format!("{blob} why does this token fail to verify?"));
        assert!(
            unbroken.ends_with("why does this token fail to verify?"),
            "{unbroken}"
        );
        let run_on = format!(
            "why does checkout fail, {} and is it the same bug as last week?",
            "the cart shows a spinner and then nothing happens, ".repeat(120)
        );
        let kept = default(&run_on);
        assert!(kept.starts_with("why does checkout fail,"), "{kept}");
        assert!(
            kept.ends_with("and is it the same bug as last week?"),
            "{kept}"
        );
        let no_question = default(&"x".repeat(9_000));
        assert!(no_question.chars().count() > 1_300, "the budget is used");
        assert!(no_question.contains(ELISION));
    }

    #[test]
    fn requests_in_other_scripts_stay_within_the_budget_with_their_questions() {
        let budget = StateBudget::chars(250);
        let japanese = built(&"注文が届きません。返金できますか？ ".repeat(200), budget);
        assert!(japanese.contains("返金できますか？"));
        let chinese = built(
            &format!(
                "{}你们什么时候发货？",
                "我上周下了订单但是还没有收到任何通知。".repeat(80)
            ),
            budget,
        );
        assert!(chinese.ends_with("你们什么时候发货？"), "{chinese}");
        let arabic = built(
            &format!(
                "{} متى يصل الطلب؟",
                "لقد طلبت المنتج الأسبوع الماضي ولم يصل بعد. ".repeat(60)
            ),
            budget,
        );
        assert!(arabic.ends_with("متى يصل الطلب؟"), "{arabic}");
        let spanish = built(
            &format!(
                "{}¿Cuándo llega mi pedido?",
                "Hice el pedido la semana pasada y no ha llegado. ".repeat(60)
            ),
            budget,
        );
        assert!(spanish.ends_with("¿Cuándo llega mi pedido?"), "{spanish}");
        let emoji = built(
            &format!(
                "{}can you help?🙏",
                "👨‍👩‍👧‍👦 our family plan stopped working 😡🔥 ".repeat(60)
            ),
            budget,
        );
        assert!(emoji.ends_with("can you help?🙏"), "{emoji}");
    }

    #[test]
    fn a_markdown_table_is_data_not_a_log() {
        let table = "| Day | Refunds | Note |\n|---|---|---|\n| 2026-09-01 | 3 | ERROR spike |\n| 2026-09-02 | 41 | INFO normal |\n| 2026-09-03 | 4 | WARN late |";
        assert_eq!(
            default(&format!("Which day had the most refunds?\n{table}")),
            format!("Which day had the most refunds?\n{table}")
        );
        let rows: String = (0..400)
            .map(|i| format!("| 2026-09-{:02} | {i} | ok |\n", i % 28 + 1))
            .collect();
        let long = default(&format!(
            "| Day | Refunds | Note |\n{rows}Which day had the most refunds?"
        ));
        assert!(long.starts_with("| Day | Refunds | Note |"), "{long}");
        assert!(long.ends_with("Which day had the most refunds?"));
    }

    #[test]
    fn a_slack_message_keeps_its_mentions_links_and_one_line_snippets() {
        let slack = "<@U04ABCDEF> <!here> the deploy failed again :fire:\n```error: connection reset```\n&gt; it worked yesterday\n&gt; and the day before\nSee <https://grafana.example.com/d/abc?orgId=1&amp;from=now-6h|the dashboard> — *can you roll it back?*";
        assert_eq!(default(slack), slack);
        let quoted: String = (0..40)
            .map(|i| format!("&gt; earlier line {i}\n"))
            .collect();
        let long = default(&format!(
            "<@U04ABCDEF> ```kubectl get pods```\n{quoted}*is this the same outage?*"
        ));
        assert_eq!(
            long,
            "<@U04ABCDEF> ```kubectl get pods```\n[40 quoted lines]\n*is this the same outage?*"
        );
    }

    #[test]
    fn a_request_that_is_only_a_paste_is_kept_as_written_not_as_a_bare_marker() {
        let forwarded = "> Can you look at invoice 42?\n> It is still unpaid\n> Thanks";
        assert_eq!(default(forwarded), forwarded);
        let query = "```sql\nSELECT count(*)\nFROM refunds\nWHERE day = '2026-09-01';\n```";
        assert_eq!(default(query), query);
    }

    #[test]
    fn an_unclosed_fence_does_not_swallow_the_question_after_it() {
        let lines: String = (0..30).map(|i| format!("row {i}\n")).collect();
        let kept = default(&format!("See:\n```\n{lines}What is wrong with these rows?"));
        assert!(kept.ends_with("What is wrong with these rows?"), "{kept}");
    }

    #[test]
    fn a_literal_marker_reads_as_what_it_says_and_is_never_collapsed_again() {
        let literal = "He wrote […] and then pasted\n[42 lines of log]\n[3 lines of code]\n[7 quoted lines]\nwhat does it mean?";
        assert_eq!(default(literal), literal);
        let log: String = (0..50)
            .map(|i| format!("2026-09-26T10:00:{i:02}Z INFO tick {i}\n"))
            .collect();
        let once = default(&format!("Look:\n{log}why?"));
        assert_eq!(default(&once), once);
    }
}
