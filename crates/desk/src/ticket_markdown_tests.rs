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

const FILE: &str = "6f1c3b8e-0000-4000-8000-000000000001";

fn md(content: Value) -> String {
    let id = Uuid::parse_str(FILE).unwrap();
    let files = [AdfFile {
        id,
        name: "shot.png",
        remote_id: Some("10001"),
    }];
    adf_markdown(&doc(content), &files)
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
        (":::panel{tone=danger}", "\\:\\:\\:panel{tone=danger}"),
        ("see :x[boom]{a=b}", "see \\:x\\[boom\\]{a=b}"),
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
