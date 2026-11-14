use serde_json::{json, Value};
use uuid::Uuid;

use super::*;

fn doc(content: Value) -> Value {
    json!({ "type": "doc", "version": 1, "content": content })
}

fn t(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn tm(text: &str, marks: Value) -> Value {
    json!({ "type": "text", "text": text, "marks": marks })
}

fn p(content: Value) -> Value {
    json!({ "type": "paragraph", "content": content })
}

fn li(content: Value) -> Value {
    json!({ "type": "listItem", "content": content })
}

/// The ticket's own address: its site's issues are chips.
const OWN: &str = "https://acme.example/browse/ACME-1";

const FILE: &str = "6f1c3b8e-0000-4000-8000-000000000001";

fn md(content: Value) -> String {
    let id = Uuid::parse_str(FILE).unwrap();
    let files = [AdfFile {
        id,
        name: "shot.png",
    }];
    adf_markdown(&doc(content), &files, OWN)
}

/// Every row of the ADF → Desk markdown table, as written.
#[test]
fn each_adf_node_reads_as_its_desk_markdown() {
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "paragraphs and headings",
            json!([
                { "type": "heading", "attrs": { "level": 2 }, "content": [t("Steps")] },
                p(json!([t("It fails.")])),
                p(json!([t("Twice.")]))
            ]),
            "## Steps\n\nIt fails.\n\nTwice.",
        ),
        (
            "strong, em, code, strike",
            json!([p(json!([
                tm("bold", json!([{ "type": "strong" }])),
                t(" and "),
                tm("it", json!([{ "type": "em" }])),
                t(", "),
                tm("a_b", json!([{ "type": "code" }])),
                t(", "),
                tm("gone", json!([{ "type": "strike" }]))
            ]))]),
            "**bold** and *it*, `a_b`, ~~gone~~",
        ),
        (
            "marks nest, spaces outside them",
            json!([p(json!([
                tm("one ", json!([{ "type": "strong" }])),
                tm("two", json!([{ "type": "strong" }, { "type": "em" }])),
                t(" three")
            ]))]),
            "**one *two*** three",
        ),
        (
            "an https link",
            json!([p(json!([tm("the doc", json!([{ "type": "link", "attrs": { "href": "https://x.test/a?b=1" } }]))]))]),
            "[the doc](https://x.test/a?b=1)",
        ),
        (
            "bullets and numbers, nested",
            json!([
                { "type": "bulletList", "content": [
                    li(json!([p(json!([t("one")])),
                        { "type": "bulletList", "content": [li(json!([p(json!([t("inner")]))]))] }])),
                    li(json!([p(json!([t("two")]))]))
                ] },
                { "type": "orderedList", "attrs": { "order": 1 }, "content": [
                    li(json!([p(json!([t("first")])),
                        { "type": "orderedList", "content": [li(json!([p(json!([t("sub")]))]))] }])),
                    li(json!([p(json!([t("second")]))]))
                ] }
            ]),
            "* one\n  * inner\n* two\n\n1. first\n   1. sub\n2. second",
        ),
        (
            "an issue's card, and a link to it",
            json!([p(json!([
                { "type": "inlineCard", "attrs": { "url": "https://acme.example/browse/ACME-875" } },
                t(" and "),
                tm("https://acme.example/browse/ACME-9", json!([{ "type": "link", "attrs": { "href": "https://acme.example/browse/ACME-9" } }]))
            ]))]),
            "[ACME-875](https://acme.example/browse/ACME-875) and [ACME-9](https://acme.example/browse/ACME-9)",
        ),
        (
            "a mention",
            json!([p(json!([t("Ask "), { "type": "mention", "attrs": { "id": "x", "text": "@Dana Lee" } }]))]),
            "Ask :mention[Dana Lee]",
        ),
        (
            "a status, its tone from its colour",
            json!([p(json!([
                { "type": "status", "attrs": { "text": "Done", "color": "green" } }, t(" "),
                { "type": "status", "attrs": { "text": "Blocked", "color": "red" } }, t(" "),
                { "type": "status", "attrs": { "text": "Doing", "color": "blue" } }, t(" "),
                { "type": "status", "attrs": { "text": "Wait", "color": "yellow" } }, t(" "),
                { "type": "status", "attrs": { "text": "Idea", "color": "purple" } }, t(" "),
                { "type": "status", "attrs": { "text": "New", "color": "neutral" } }
            ]))]),
            ":status[Done]{tone=success} :status[Blocked]{tone=danger} :status[Doing]{tone=info} :status[Wait]{tone=warning} :status[Idea]{tone=purple} :status[New]{tone=neutral}",
        ),
        (
            "a date",
            json!([p(json!([t("Due "), { "type": "date", "attrs": { "timestamp": "1795996800000" } }]))]),
            "Due :date[2026-11-30]",
        ),
        (
            "panels",
            json!([
                { "type": "panel", "attrs": { "panelType": "info" }, "content": [p(json!([t("Heads up.")]))] },
                { "type": "panel", "attrs": { "panelType": "note" }, "content": [p(json!([t("n")]))] },
                { "type": "panel", "attrs": { "panelType": "success" }, "content": [p(json!([t("s")]))] },
                { "type": "panel", "attrs": { "panelType": "warning" }, "content": [p(json!([t("w")]))] },
                { "type": "panel", "attrs": { "panelType": "error" }, "content": [p(json!([t("e")]))] }
            ]),
            ":::panel{tone=info}\nHeads up.\n:::\n\n:::panel{tone=purple}\nn\n:::\n\n:::panel{tone=success}\ns\n:::\n\n:::panel{tone=warning}\nw\n:::\n\n:::panel{tone=danger}\ne\n:::",
        ),
        (
            "an expand, and one inside a panel",
            json!([
                { "type": "expand", "attrs": { "title": "Logs" }, "content": [p(json!([t("x")]))] },
                { "type": "panel", "attrs": { "panelType": "info" }, "content": [
                    { "type": "nestedExpand", "attrs": { "title": "More" }, "content": [p(json!([t("y")]))] }
                ] }
            ]),
            ":::details[Logs]\nx\n:::\n\n::::panel{tone=info}\n:::details[More]\ny\n:::\n::::",
        ),
        (
            "tasks",
            json!([{ "type": "taskList", "content": [
                { "type": "taskItem", "attrs": { "state": "TODO" }, "content": [t("write it")] },
                { "type": "taskItem", "attrs": { "state": "DONE" }, "content": [t("plan it")] },
                { "type": "taskList", "content": [
                    { "type": "taskItem", "attrs": { "state": "TODO" }, "content": [t("sub")] }
                ] }
            ] }]),
            "- [ ] write it\n- [x] plan it\n  - [ ] sub",
        ),
        (
            "a table: first row the header, a cell on one line, short rows filled",
            json!([{ "type": "table", "content": [
                { "type": "tableRow", "content": [
                    { "type": "tableHeader", "content": [p(json!([t("Name")]))] },
                    { "type": "tableHeader", "content": [p(json!([t("Note")]))] }
                ] },
                { "type": "tableRow", "content": [
                    { "type": "tableCell", "content": [p(json!([t("a|b")]))] },
                    { "type": "tableCell", "content": [p(json!([t("one")])), p(json!([t("two")]))] }
                ] },
                { "type": "tableRow", "content": [
                    { "type": "tableCell", "content": [p(json!([t("solo")]))] }
                ] }
            ] }]),
            "| Name | Note |\n| --- | --- |\n| a\\|b | one two |\n| solo | |",
        ),
        (
            "code, with its language",
            json!([{ "type": "codeBlock", "attrs": { "language": "rust" }, "content": [t("let x = 1;\n*not* [md]")] }]),
            "```rust\nlet x = 1;\n*not* [md]\n```",
        ),
        (
            "a picture of the ticket's, by its row's id",
            json!([{ "type": "mediaSingle", "content": [
                { "type": "media", "attrs": { "id": "694f658b-26d6-4de1-b2cb-b2bfdf43679a", "type": "file", "collection": "", "alt": "shot.png" } }
            ] }]),
            "![shot.png](desk-file:6f1c3b8e-0000-4000-8000-000000000001)",
        ),
        (
            "a picture not among its files: its name",
            json!([{ "type": "mediaSingle", "content": [
                { "type": "media", "attrs": { "id": "x", "type": "file", "alt": "gone.png" } }
            ] }]),
            "gone.png",
        ),
        (
            "a rule",
            json!([p(json!([t("a")])), { "type": "rule" }, p(json!([t("b")]))]),
            "a\n\n---\n\nb",
        ),
        (
            "an emoji: its character, else its short name escaped",
            json!([p(json!([
                { "type": "emoji", "attrs": { "shortName": ":tada:", "text": "🎉" } }, t(" "),
                { "type": "emoji", "attrs": { "shortName": ":party-parrot:" } }
            ]))]),
            "🎉 \\:party-parrot:",
        ),
        (
            "a line break",
            json!([p(json!([t("one"), { "type": "hardBreak" }, t("- two")]))]),
            "one  \n\\- two",
        ),
        (
            "anything unknown keeps its words",
            json!([
                { "type": "layoutSection", "content": [
                    { "type": "layoutColumn", "content": [p(json!([t("left")]))] },
                    { "type": "layoutColumn", "content": [p(json!([t("right")]))] }
                ] },
                p(json!([t("a "), { "type": "futureInline", "content": [t("kept")] }, t(" "),
                    { "type": "otherInline", "attrs": { "text": "also" } }])),
                { "type": "decisionList", "content": [
                    { "type": "decisionItem", "attrs": { "state": "DECIDED" }, "content": [t("ship it")] }
                ] }
            ]),
            "left\n\nright\n\na kept also\n\n- ship it",
        ),
    ];
    for (what, content, want) in cases {
        assert_eq!(md(content), want, "{what}");
    }
}

/// Someone's words never open a directive, a fence, emphasis, a link or
/// a block of their own: each reads back as just the words.
#[test]
fn words_never_open_markdown_of_their_own() {
    let cases = [
        (":::panel{tone=danger}", "\\:\\:\\:panel\\{tone=danger}"),
        ("see :x[boom]{a=b}", "see \\:x\\[boom\\]\\{a=b}"),
        ("```js", "\\`\\`\\`js"),
        ("~~~", "\\~\\~\\~"),
        ("*not bold*", "\\*not bold\\*"),
        ("_edge_ and snake_case", "\\_edge\\_ and snake_case"),
        ("[a](https://x.test)", "\\[a\\](https://x.test)"),
        ("<b>tag</b> &amp;", "\\<b>tag\\</b> \\&amp;"),
        ("# not a heading", "\\# not a heading"),
        ("> not a quote", "\\> not a quote"),
        ("- not a list", "\\- not a list"),
        ("1. not a list", "1\\. not a list"),
        ("    not code", "not code"),
        ("at 10:30, a\\b", "at 10:30, a\\\\b"),
    ];
    for (words, want) in cases {
        assert_eq!(md(json!([p(json!([t(words)]))])), want, "{words}");
    }
    // In a directive's label too.
    assert_eq!(
        md(json!([p(
            json!([{ "type": "mention", "attrs": { "text": "@a]b[:c" } }])
        )])),
        ":mention[a\\]b\\[\\:c]"
    );
    assert_eq!(
        md(
            json!([{ "type": "expand", "attrs": { "title": "x] y" }, "content": [p(json!([t("z")]))] }])
        ),
        ":::details[x\\] y]\nz\n:::"
    );
}

/// A link keeps its address only when it is `https` and safe to write;
/// its words stay either way.
#[test]
fn only_https_links_keep_their_address() {
    let link = |href: &str| {
        md(json!([p(json!([tm(
            "words",
            json!([{ "type": "link", "attrs": { "href": href } }])
        )]))]))
    };
    assert_eq!(link("https://x.test/a"), "[words](https://x.test/a)");
    assert_eq!(link("http://x.test/a"), "words");
    assert_eq!(link("javascript:alert(1)"), "words");
    assert_eq!(link("https://x.test/a)b"), "words");
    assert_eq!(link("file:///etc/passwd"), "words");
    let card = |url: &str| {
        md(json!([p(
            json!([{ "type": "inlineCard", "attrs": { "url": url } }])
        )]))
    };
    assert_eq!(card("https://x.test/page"), "<https://x.test/page>");
    assert_eq!(card("javascript:alert(1)"), "javascript\\:alert(1)");
}

/// Code is fenced with more backticks than any run inside it, so nothing
/// in it closes it early.
#[test]
fn a_code_fence_is_longer_than_any_run_inside() {
    let code = |text: &str| md(json!([{ "type": "codeBlock", "content": [t(text)] }]));
    assert_eq!(code("a ``` b"), "````\na ``` b\n````");
    assert_eq!(code("`````"), "``````\n`````\n``````");
    assert_eq!(code("plain"), "```\nplain\n```");
    let lang = md(
        json!([{ "type": "codeBlock", "attrs": { "language": "js\n```x" }, "content": [t("1")] }]),
    );
    assert_eq!(
        lang, "```jsx\n1\n```",
        "the language keeps only safe characters"
    );
    let span = |text: &str| md(json!([p(json!([tm(text, json!([{ "type": "code" }]))]))]));
    assert_eq!(span("a`b"), "``a`b``");
    assert_eq!(span("`x"), "`` `x ``");
}

/// Bare keys of the issue's own team and Linear issue addresses become
/// links; code, links and other teams' keys stay as they are, and reading
/// it twice changes nothing.
#[test]
fn a_linear_description_links_its_issues() {
    let url = "https://linear.app/acme/issue/MAN-1/fix-it";
    let cases = [
        (
            "See MAN-611 and OPS-2.",
            "See [MAN-611](https://linear.app/acme/issue/MAN-611) and OPS-2.",
        ),
        (
            "Was https://linear.app/acme/issue/MAN-7/old-title.",
            "Was [MAN-7](https://linear.app/acme/issue/MAN-7/old-title).",
        ),
        (
            "<https://linear.app/acme/issue/MAN-8>",
            "[MAN-8](https://linear.app/acme/issue/MAN-8)",
        ),
        (
            "[read](https://linear.app/acme/issue/MAN-9) `MAN-10` xMAN-11 MAN-12a",
            "[read](https://linear.app/acme/issue/MAN-9) `MAN-10` xMAN-11 MAN-12a",
        ),
        ("https://example.test/MAN-13", "https://example.test/MAN-13"),
        ("```\nMAN-14\n```", "```\nMAN-14\n```"),
    ];
    for (input, want) in cases {
        let once = linear_markdown(input, "MAN-1", url);
        assert_eq!(once, want, "{input}");
        assert_eq!(linear_markdown(&once, "MAN-1", url), once, "twice: {input}");
    }
    // Without the issue's own address, a bare key stays bare.
    assert_eq!(linear_markdown("MAN-3", "MAN-1", ""), "MAN-3");
}

/// `+++ Title` … `+++` is a collapsible, nested ones fenced longer, and it
/// goes back to Linear as it came.
#[test]
fn a_linear_collapsible_reads_as_details_and_goes_back() {
    let input = "Intro\n+++ Logs [raw]\nline\n+++ Inner\nx\n+++\n+++\nafter\n+++ never closed";
    let got = linear_markdown(input, "MAN-1", "");
    assert_eq!(
        got,
        "Intro\n::::details[Logs \\[raw\\]]\nline\n:::details[Inner]\nx\n:::\n::::\nafter\n+++ never closed"
    );
    assert_eq!(linear_markdown(&got, "MAN-1", ""), got);
    assert_eq!(linear_out(&got), input);
    // Other containers and code stay as they are.
    let other = ":::panel{tone=info}\nx\n:::\n```\n:::details[a]\n```";
    assert_eq!(linear_out(other), other);
}

/// A description read as ADF and edited here goes back to Jira with its
/// directives as plain markdown; code, links and escapes stay as they are.
#[test]
fn an_edited_description_goes_back_with_plain_markdown() {
    let cases = [
        ("Ask :mention[Dana Lee] now", "Ask @Dana Lee now"),
        (
            ":status[Done]{tone=success} on :date[2026-11-30]",
            "[Done] on 2026-11-30",
        ),
        (
            "![shot.png](desk-file:6f1c3b8e-0000-4000-8000-000000000001)",
            "shot.png",
        ),
        (
            "[ACME-2](https://acme.example/browse/ACME-2)",
            "[ACME-2](https://acme.example/browse/ACME-2)",
        ),
        (
            "`:mention[x]` and \\:mention\\[y\\] and a:mention[z]",
            "`:mention[x]` and \\:mention\\[y\\] and a:mention[z]",
        ),
        (
            ":::panel{tone=info}\nHeads up.\n\nTwo.\n:::\nafter",
            "> Heads up.\n>\n> Two.\nafter",
        ),
        (
            "::::panel{tone=warning}\n:::details[Logs]\nx\n:::\n::::",
            "> **Logs**\n> x",
        ),
        ("```\n:mention[kept]\n```", "```\n:mention[kept]\n```"),
        (":::other\nx\n:::", ":::other\nx\n:::"),
    ];
    for (md, want) in cases {
        assert_eq!(jira_out(md), want, "{md}");
    }
}

/// Words right after a mention or a date never become its attributes.
#[test]
fn words_after_a_directive_never_become_its_attributes() {
    assert_eq!(
        md(json!([p(json!([
            { "type": "mention", "attrs": { "text": "@Bob" } },
            t("{href=javascript:x}")
        ]))])),
        ":mention[Bob]\\{href=javascript\\:x}"
    );
    assert_eq!(
        md(json!([p(json!([
            { "type": "date", "attrs": { "timestamp": "1795996800000" } },
            t("{x}")
        ]))])),
        ":date[2026-11-30]\\{x}"
    );
}

/// Only an issue on the ticket's own site is a chip; any other is its
/// link as it is.
#[test]
fn only_the_ticket_s_own_site_s_issues_are_chips() {
    let card = |url: &str| {
        md(json!([p(
            json!([{ "type": "inlineCard", "attrs": { "url": url } }])
        )]))
    };
    assert_eq!(
        card("https://ACME.example/browse/ACME-9"),
        "[ACME-9](https://ACME.example/browse/ACME-9)"
    );
    assert_eq!(
        card("https://other.example/browse/ACME-9"),
        "<https://other.example/browse/ACME-9>"
    );
    let link = md(json!([p(json!([tm(
        "https://other.example/browse/ACME-9",
        json!([{ "type": "link", "attrs": { "href": "https://other.example/browse/ACME-9" } }])
    )]))]));
    assert_eq!(
        link,
        "[https://other.example/browse/ACME-9](https://other.example/browse/ACME-9)"
    );
}

/// In a table cell, a `|` in an address is encoded so it can't end the
/// cell.
#[test]
fn an_address_in_a_cell_never_ends_it() {
    let got = md(json!([{ "type": "table", "content": [
        { "type": "tableRow", "content": [
            { "type": "tableHeader", "content": [p(json!([
                tm("q", json!([{ "type": "link", "attrs": { "href": "https://x.test/?a|b" } }])),
                t(" "),
                { "type": "inlineCard", "attrs": { "url": "https://x.test/c|d" } }
            ]))] }
        ] }
    ] }]));
    assert_eq!(
        got,
        "| [q](https://x.test/?a%7Cb) <https://x.test/c%7Cd> |\n| --- |"
    );
}

/// A picture's caption keeps its words, under it.
#[test]
fn a_picture_keeps_its_caption() {
    let got = md(json!([{ "type": "mediaSingle", "content": [
        { "type": "media", "attrs": { "type": "file", "alt": "shot.png" } },
        { "type": "caption", "content": [t("The *login* page")] }
    ] }]));
    assert_eq!(
        got,
        format!("![shot.png](desk-file:{FILE})\n\nThe \\*login\\* page")
    );
}

/// Hostile lines (100 000 characters of what each reader looks ahead on)
/// are read in a moment, and deep nesting is read whole.
#[test]
fn hostile_text_is_read_in_a_moment() {
    let n = 100_000;
    let lines = [
        "[".repeat(n),
        "[a".repeat(n / 2),
        "[a](x".repeat(n / 5),
        "<".repeat(n),
        "<a".repeat(n / 2),
        "`".repeat(n),
        "`a``a```a````".repeat(n / 13),
        "ab ".repeat(n / 3),
        "a+".repeat(n / 2),
        "a.b-".repeat(n / 4),
        "MAN-1 ".repeat(n / 6),
        "https://linear.app/acme/issue/MAN-2 ".repeat(n / 37),
        ":mention[".repeat(n / 9),
        "![a](desk-file:".repeat(n / 15),
        ":status[a]{".repeat(n / 11),
    ];
    let started = std::time::Instant::now();
    for l in &lines {
        let _ = linear_markdown(l, "MAN-1", "https://linear.app/acme/issue/MAN-1");
        let _ = jira_out(l);
        let _ = md(json!([p(json!([t(l)]))]));
    }
    let many = "+++ x\n".repeat(n / 6) + &"+++\n".repeat(n / 6);
    let _ = linear_markdown(&many, "MAN-1", "");
    let _ = jira_out(&":::panel{tone=info}\n".repeat(n / 20));
    let took = started.elapsed();
    assert!(took < std::time::Duration::from_secs(5), "took {took:?}");

    // Panels inside panels, 100 deep: every word is kept.
    let mut node = p(json!([t("deep")]));
    for _ in 0..100 {
        node = json!({ "type": "panel", "attrs": { "panelType": "info" }, "content": [node] });
    }
    let got = md(json!([node]));
    assert!(got.contains("\ndeep\n"), "{got}");
    assert!(
        got.starts_with(&":".repeat(102)),
        "the outer fence is the longest"
    );
}

/// A Linear description's profile of its own workspace, bare, an autolink
/// or a link, reads as a mention of the member it is, and goes back to
/// Linear as it came; any other profile, an unknown one, one in code or a
/// picture, or right after a word, as it is.
#[test]
fn a_linear_profile_reads_as_a_mention_and_goes_back() {
    let url = "https://linear.app/acme/issue/MAN-1/fix-it";
    let ws = linear_workspace(url).unwrap();
    assert_eq!(ws, "acme");
    let names: HashMap<String, String> = [
        ("ann".to_string(), "Ann Lee".to_string()),
        ("bo.k".to_string(), "Bo [K]".to_string()),
    ]
    .into();
    let cases = [
        (
            "Ask https://linear.app/acme/profiles/ann.",
            "Ask :mention[Ann Lee]{url=\"https://linear.app/acme/profiles/ann\"}.",
            "Ask https://linear.app/acme/profiles/ann.",
        ),
        (
            "cc [@Bo](https://linear.app/acme/profiles/bo.k) and <https://linear.app/acme/profiles/ann>",
            "cc :mention[Bo \\[K\\]]{url=\"https://linear.app/acme/profiles/bo.k\" text=\"@Bo\"} and :mention[Ann Lee]{url=\"https://linear.app/acme/profiles/ann\"}",
            "cc [@Bo](https://linear.app/acme/profiles/bo.k) and https://linear.app/acme/profiles/ann",
        ),
    ];
    for (input, want, back) in cases {
        let read = linear_mentions(&linear_markdown(input, "MAN-1", url), ws, &names);
        assert_eq!(read, want, "{input}");
        assert_eq!(linear_out(&read), back, "back: {input}");
    }
    let untouched = [
        // Another workspace's, an unknown member, in code, a picture,
        // right after a word, words that can't be held quoted.
        "https://linear.app/other/profiles/ann",
        "https://linear.app/acme/profiles/zed",
        "`https://linear.app/acme/profiles/ann`",
        "```\nhttps://linear.app/acme/profiles/ann\n```",
        "![me](https://linear.app/acme/profiles/ann)",
        "x[Ann](https://linear.app/acme/profiles/ann)",
        "see:https://linear.app/acme/profiles/ann",
        "[say \"hi\"](https://linear.app/acme/profiles/ann)",
        "https://linear.app/acme/profiles/ann/activity",
    ];
    for input in untouched {
        let read = linear_mentions(input, ws, &names);
        assert_eq!(read, input);
        assert_eq!(linear_out(&read), input);
    }
    // No names found: nothing changes.
    let input = "Ask https://linear.app/acme/profiles/ann";
    assert_eq!(linear_mentions(input, ws, &HashMap::new()), input);
    // What is looked up: each member once, outside code, at most `max`.
    assert_eq!(
        linear_profile_slugs(
            "https://linear.app/acme/profiles/ann [A](https://linear.app/acme/profiles/ann) `https://linear.app/acme/profiles/cy` https://linear.app/acme/profiles/bo.k https://linear.app/acme/profiles/dee",
            ws,
            2
        ),
        vec!["ann".to_string(), "bo.k".to_string()]
    );
    // A mention typed in Desk, with no profile behind it, goes as typed.
    assert_eq!(linear_out(":mention[Ann]"), ":mention[Ann]");
    // Read again, a mention stays one.
    let once = linear_mentions("https://linear.app/acme/profiles/ann", ws, &names);
    assert_eq!(linear_mentions(&once, ws, &names), once);
}
