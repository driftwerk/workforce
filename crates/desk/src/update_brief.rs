//! A boss's update, briefed for the places that are not Desk's own chat:
//! Slack (Block Kit) and the Floor (one plain line). Same rules as the
//! chat's brief:
//!
//! - only the critical facts: the headline, then one line per item,
//!   grouped by session;
//! - `✓` for what it did, with why in a few words;
//! - what only the person can do is one line, closed with Done alone;
//! - never a suggestion, a recommendation or an answer button;
//! - what was settled in its session collapses away, and a session the
//!   person is in now is not spoken of at all.
//!
//! What a remote boss says in its Slack thread is read into the same shape
//! ([`parse_reply`]), its lengths held in code, so a free-form turn can't
//! drift into prose.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::crew::{clip, one_line, words};
use crate::decision::{BRIEF_CHARS, BRIEF_WORDS, WHY_WORDS};
use crate::update::{
    headline_line, plain_headline, BriefGroup, BriefItem, BriefKind, MentionedUpdate, BRIEF_ITEMS,
    CREW_LABEL, HEADLINE_WORDS,
};

/// What of `u` may be said outside the chat: the groups, lines, questions
/// and sessions about a session `hidden` names (the person is in it) left
/// out, every line and question settled since collapsed away, and a group
/// left with no line dropped (what it left alone is only a count, and no
/// critical fact).
pub fn shown(u: &MentionedUpdate, hidden: &dyn Fn(&str) -> bool) -> MentionedUpdate {
    let about_hidden = |s: &Option<String>| s.as_deref().is_some_and(hidden);
    let brief: Vec<BriefGroup> = u
        .brief
        .iter()
        .filter(|g| !about_hidden(&g.session_id))
        .map(|g| BriefGroup {
            items: g
                .items
                .iter()
                .filter(|i| i.settled_by.is_none())
                .cloned()
                .collect(),
            left_alone: 0,
            ..g.clone()
        })
        .filter(|g| !g.items.is_empty())
        .collect();
    let hid = brief.len() != u.brief.iter().filter(|g| !g.items.is_empty()).count()
        || brief.iter().map(|g| g.items.len()).sum::<usize>()
            != u.brief.iter().map(|g| g.items.len()).sum::<usize>()
        || u.sessions.iter().any(|s| hidden(&s.session_id));
    let mut out = MentionedUpdate {
        sessions: u
            .sessions
            .iter()
            .filter(|s| !hidden(&s.session_id))
            .cloned()
            .collect(),
        needs_you: u
            .needs_you
            .iter()
            .filter(|n| n.settled_by.is_none() && !about_hidden(&n.session_id))
            .cloned()
            .collect(),
        brief,
        ..u.clone()
    };
    // A headline written over lines no longer shown would say them: it is
    // said again from what is.
    let needs_hid = out.needs_you.len() != u.needs_you.len();
    if hid || needs_hid {
        // What was left out may be about what is no longer said: not
        // counted either.
        out.more_items = 0;
    }
    if hid || needs_hid || out.headline.trim().is_empty() {
        let for_you = out
            .brief
            .iter()
            .flat_map(|g| &g.items)
            .filter(|i| i.kind == BriefKind::ForYou)
            .count();
        out.headline = plain_headline(&out, for_you);
    }
    out
}

/// Whether [`shown`] left out anything of `u`, so that `shown` is all that
/// may be said: no headline of `u`'s own over it.
pub fn hides(u: &MentionedUpdate, shown: &MentionedUpdate) -> bool {
    let items = |u: &MentionedUpdate| u.brief.iter().map(|g| g.items.len()).sum::<usize>();
    items(u) != items(shown)
        || u.needs_you.len() != shown.needs_you.len()
        || u.sessions.len() != shown.sessions.len()
}

/// Whether `u` (already [`shown`]) says anything at all.
pub fn says_anything(u: &MentionedUpdate) -> bool {
    !u.brief.is_empty() || !u.needs_you.is_empty() || !u.sessions.is_empty() || !u.kept.is_empty()
}

/// `s` as Slack mrkdwn shows it plainly: `&`, `<` and `>` escaped so no
/// link, mention or broadcast in it works, and its emphasis marks and
/// backticks left out, on one line.
pub fn mrkdwn(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let word = |i: Option<usize>| {
        i.and_then(|i| chars.get(i))
            .is_some_and(|c| c.is_alphanumeric())
    };
    for (n, &c) in chars.iter().enumerate() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // Inside a word (`snake_case`) an underscore is the name's own.
            '_' if word(n.checked_sub(1)) && word(Some(n + 1)) => out.push('_'),
            '*' | '_' | '~' | '`' => {}
            '\n' | '\r' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// One brief line, as text: `✓ action — why` for what it did,
/// `● For you: action — why` for what only the person can do.
fn line(i: &BriefItem) -> String {
    let mark = match i.kind {
        BriefKind::Done => "✓",
        BriefKind::ForYou => "● For you:",
    };
    match i.why.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        Some(why) => format!("{mark} {} — {}", mrkdwn(&i.action), mrkdwn(why)),
        None => format!("{mark} {}", mrkdwn(&i.action)),
    }
}

/// `u` (already [`shown`]) as a Slack message: its notification text and
/// its blocks. A section for the headline, one per session's group (its
/// name, then one line per item), what only the person can do as a
/// context line ending in its Done link when `done` gives one, what was
/// left out as a count, and each question waiting on the person as one
/// line. Never an `actions` block or a button: nothing is answered here.
pub fn slack_blocks(
    u: &MentionedUpdate,
    done: &dyn Fn(&BriefItem) -> Option<String>,
) -> (String, Vec<Value>) {
    let headline = mrkdwn(&u.headline);
    let mut blocks = Vec::new();
    if !headline.is_empty() {
        blocks.push(section(&format!("*{headline}*")));
    }
    for g in &u.brief {
        let did: Vec<String> = g
            .items
            .iter()
            .filter(|i| i.kind == BriefKind::Done)
            .map(line)
            .collect();
        let label = format!("*{}*", mrkdwn(&g.label));
        blocks.push(section(
            &std::iter::once(label)
                .chain(did)
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        for i in g.items.iter().filter(|i| i.kind == BriefKind::ForYou) {
            let text = match done(i) {
                Some(url) => format!("{} · <{url}|Done>", line(i)),
                None => line(i),
            };
            blocks.push(context(&text));
        }
    }
    for n in &u.needs_you {
        blocks.push(context(&format!("● Needs you: {}", mrkdwn(&n.question))));
    }
    if u.more_items > 0 {
        blocks.push(context(&format!("+{} more in Desk", u.more_items)));
    }
    (headline, blocks)
}

fn section(text: &str) -> Value {
    json!({"type": "section", "text": {"type": "mrkdwn", "text": text}})
}

fn context(text: &str) -> Value {
    json!({"type": "context", "elements": [{"type": "mrkdwn", "text": text}]})
}

/// What the Floor says of the boss as it sends `u` (already [`shown`]):
/// its purpose and where things stand, in plain words, no ids. A
/// catch-up (`away`, its own headline) says so, and its headline is the
/// state when it has one.
pub fn floor_line(u: &MentionedUpdate, away: Option<&str>) -> (String, String) {
    let purpose = match away {
        Some(_) => "Catching you up on the crew.",
        None => "Keeping you posted on the crew.",
    };
    let said = away
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or(&u.headline);
    let mut state = clip(&said.split_whitespace().collect::<Vec<_>>().join(" "), 140);
    if state.is_empty() {
        state = "Nothing new.".into();
    }
    // The Floor reads a question as one waiting on the person: what waits
    // is the update's own card.
    let state = match state.strip_suffix('?') {
        Some(s) => format!("{}.", s.trim_end_matches('?')),
        None => state,
    };
    let state = match state.ends_with(['.', '!', '…']) {
        true => state,
        false => format!("{state}."),
    };
    (purpose.to_string(), state)
}

/// The lines a boss's Slack post is asked to be written in, for its
/// prompt.
pub const REPLY_FORM: &str = "Answer in this form only, nothing else:\n\
<the whole update in at most 12 words>\n\
[<session name>]\n\
✓ <what you did, at most 12 words> — <why, at most 8 words>\n\
For you: <what only the person can do, at most 12 words> — <why, a few words>\n\
One line per item, under its session's name in brackets; at most 3 lines in all, what only the person can do first. \
Only the critical facts: no plans, no suggestions, no options, no questions, no quotes of messages.";

/// A boss's free-form answer `reply`, read into the brief's shape (see
/// [`REPLY_FORM`]) at `at`: its headline, its `✓` and "For you" lines under
/// their sessions' names, each held to its length, what only the person
/// can do first, [`BRIEF_ITEMS`] in all and the rest counted. Anything else
/// it says (a suggestion, a plan, a question) is left out. An answer not in
/// the form at all is its first sentence as the headline.
pub fn parse_reply(reply: &str, at: DateTime<Utc>) -> MentionedUpdate {
    let mut headline: Option<String> = None;
    let mut label: Option<String> = None;
    let mut items: Vec<(Option<String>, BriefItem)> = Vec::new();
    for raw in reply.lines() {
        // A fence around the form is no part of it.
        if raw.trim().starts_with("```") {
            continue;
        }
        let mut l = bare(raw);
        if l.is_empty() {
            continue;
        }
        // `[name]`, alone or opening its line.
        // `[name]` alone, or opening a done or for-you line; never a
        // link (`[x](…)`), a checkbox or a tag in prose (`[WIP] …`).
        if let Some((name, rest)) = l.strip_prefix('[').and_then(|r| r.split_once(']')) {
            let rest = bare(rest);
            if !rest.starts_with('(') && (rest.is_empty() || marked(rest)) {
                let name = bare(name);
                label = (!name.is_empty() && !name.eq_ignore_ascii_case(CREW_LABEL))
                    .then(|| clip(name, 60));
                l = rest;
                if l.is_empty() {
                    continue;
                }
            }
        }
        let item = |kind: BriefKind, rest: &str| {
            let (action, why) = split_why(rest);
            let action = fit(action, BRIEF_WORDS, BRIEF_CHARS);
            let why = why
                .map(|w| words(w, WHY_WORDS).trim_end_matches('.').to_string())
                .filter(|w| !w.is_empty());
            (!action.is_empty()).then(|| BriefItem {
                id: String::new(),
                kind,
                action,
                why,
                detail: None,
                undo: None,
                settled_by: None,
                at,
            })
        };
        let (done, for_you) = (done_of(l), for_you_of(l));
        let parsed = match (done, for_you) {
            // A suggestion is never something done.
            (Some(rest), _) if suggests(rest) => None,
            (Some(rest), _) => item(BriefKind::Done, rest),
            (None, Some(rest)) => item(BriefKind::ForYou, rest),
            (None, None) => {
                if headline.is_none() && items.is_empty() && label.is_none() && !suggests(l) {
                    headline = headline_line(l);
                }
                None
            }
        };
        if let Some(i) = parsed {
            items.push((label.clone(), i));
        }
    }
    // What only the person can do first, then what it did, in order said.
    items.sort_by_key(|(_, i)| i.kind != BriefKind::ForYou);
    let more = items.len().saturating_sub(BRIEF_ITEMS) as u32;
    let mut brief: Vec<BriefGroup> = Vec::new();
    for (n, (label, mut i)) in items.into_iter().take(BRIEF_ITEMS).enumerate() {
        i.id = format!("line-{}", n + 1);
        let name = label.unwrap_or_else(|| CREW_LABEL.to_string());
        match brief.iter_mut().find(|g| g.label == name) {
            Some(g) => g.items.push(i),
            None => brief.push(BriefGroup {
                session_id: None,
                label: name,
                items: vec![i],
                left_alone: 0,
            }),
        }
    }
    let mut u = MentionedUpdate {
        brief,
        more_items: more,
        ..Default::default()
    };
    u.headline = match headline {
        Some(h) => h,
        None if u.brief.is_empty() => words(&one_line(reply, usize::MAX), HEADLINE_WORDS),
        None => {
            let for_you = u
                .brief
                .iter()
                .flat_map(|g| &g.items)
                .filter(|i| i.kind == BriefKind::ForYou)
                .count();
            plain_headline(&u, for_you)
        }
    };
    u
}

/// `s` within `max_words` words and `max_chars` characters, cut at a word
/// with an ellipsis, no full stop.
fn fit(s: &str, max_words: usize, max_chars: usize) -> String {
    let all: Vec<&str> = s.split_whitespace().collect();
    let mut n = all.len().min(max_words);
    let cut = |n: usize| {
        let t = all[..n].join(" ");
        let t = t
            .trim_end_matches([',', ';', ':', '.', '—', '-'])
            .to_string();
        match n < all.len() {
            true => format!("{t}…"),
            false => t,
        }
    };
    while n > 1 && cut(n).chars().count() > max_chars {
        n -= 1;
    }
    match n {
        0 => String::new(),
        _ => clip(&cut(n), max_chars),
    }
}

/// `s` with its list mark (`-`, `*`, `•`, `1.`, `2)`), emphasis marks
/// and the variation selector an emoji leaves trimmed from both ends.
fn bare(s: &str) -> &str {
    let mut s = s.trim();
    loop {
        let before = s.len();
        s = s.trim_matches(['*', '~', '\u{fe0f}']).trim();
        // A list mark is one followed by a space: `-5%` and `3.5x` are
        // the line's own.
        for mark in ["- ", "• ", "[ ] ", "[x] ", "[X] "] {
            if let Some(rest) = s.strip_prefix(mark) {
                s = rest.trim();
            }
        }
        let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 {
            let rest = &s[digits..];
            if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
                s = rest.trim();
            }
        }
        if s.len() == before {
            return s;
        }
    }
}

const DONE_MARKS: [&str; 6] = ["✔️", "✅", "✓", "✔", "☑️", "☑"];

/// What a done line says after its mark.
fn done_of(l: &str) -> Option<&str> {
    DONE_MARKS.iter().find_map(|m| l.strip_prefix(m)).map(bare)
}

/// What a for-you line says after "For you" (or "Needs you") and its
/// colon or dash.
fn for_you_of(l: &str) -> Option<&str> {
    ["● for you", "for you", "● needs you", "needs you"]
        .iter()
        .find_map(|p| strip_ci(l, p))
        .map(str::trim_start)
        .filter(|r| r.starts_with([':', '—', '–', '-']))
        .map(|r| bare(r.trim_start_matches([':', '—', '–', '-'])))
}

/// Whether `l` is a done or for-you line.
fn marked(l: &str) -> bool {
    done_of(l).is_some() || for_you_of(l).is_some()
}

/// Whether `l` is a suggestion, a plan or a question, never a fact.
fn suggests(l: &str) -> bool {
    let l = l.trim();
    let lower = l.to_lowercase();
    let opens = |p: &str| {
        lower
            .strip_prefix(p)
            .is_some_and(|r| r.is_empty() || r.starts_with([' ', ',', ':']))
    };
    l.ends_with('?')
        || [
            "i suggest",
            "i'd suggest",
            "i would suggest",
            "suggest",
            "i recommend",
            "i'd recommend",
            "maybe",
            "perhaps",
            "you could",
            "you might",
            "we could",
            "we should",
            "i could",
            "i can",
            "next i",
            "i will",
            "i'll",
            "here's",
            "here is",
            "options",
            "option",
        ]
        .iter()
        .any(|p| opens(p))
}

/// `s` without `prefix`, matched ignoring ASCII case.
fn strip_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// A line's action and its why, after its dash.
fn split_why(rest: &str) -> (&str, Option<&str>) {
    let split = rest
        .split_once(" — ")
        .or_else(|| rest.split_once(" – "))
        // A spaced hyphen may be the action's own: its why is after the last.
        .or_else(|| rest.rsplit_once(" - "));
    match split {
        Some((a, w)) => (a.trim(), Some(w.trim())),
        None => (rest.trim(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{AnswerOption, NeedsYou, SettledBy, UpdateSession};

    fn item(kind: BriefKind, action: &str, why: Option<&str>) -> BriefItem {
        BriefItem {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            action: action.into(),
            why: why.map(str::to_string),
            detail: Some("The long reason, behind more.".into()),
            undo: Some("undo".into()),
            settled_by: None,
            at: Utc::now(),
        }
    }

    fn group(sid: &str, label: &str, items: Vec<BriefItem>) -> BriefGroup {
        BriefGroup {
            session_id: Some(sid.into()),
            label: label.into(),
            items,
            left_alone: 2,
        }
    }

    const WEB: &str = "11111111-1111-4111-8111-111111111111";
    const API: &str = "22222222-2222-4222-8222-222222222222";
    const DOCS: &str = "33333333-3333-4333-8333-333333333333";

    /// A typical update at Act fully, over three sessions: what it did in
    /// two, what only the person can do in the third, and a question that
    /// came with answers (Act fully's own offer: none recommended).
    fn typical() -> MentionedUpdate {
        MentionedUpdate {
            headline: "Unblocked web's release; one thing for you.".into(),
            brief: vec![
                group(
                    DOCS,
                    "acme-docs",
                    vec![item(
                        BriefKind::ForYou,
                        "Rotate the staging deploy key in your vault",
                        Some("the old one leaked"),
                    )],
                ),
                group(
                    WEB,
                    "acme-web",
                    vec![item(
                        BriefKind::Done,
                        "Lifted my hold on tonight's release",
                        Some("CI is green again"),
                    )],
                ),
                group(
                    API,
                    "acme-api",
                    vec![item(BriefKind::Done, "Restarted the stuck test run", None)],
                ),
            ],
            needs_you: vec![NeedsYou {
                item_id: uuid::Uuid::new_v4().to_string(),
                question: "Drop the orders table or migrate it?".into(),
                items: vec!["The fixtures are a week old".into()],
                recommendation: None,
                options: vec![
                    AnswerOption {
                        label: "Drop".into(),
                        recommended: false,
                    },
                    AnswerOption {
                        label: "Migrate".into(),
                        recommended: false,
                    },
                ],
                not_blocking: None,
                age: "5 min".into(),
                generalizes: None,
                prefer_scope: None,
                session_id: Some(API.into()),
                detail: None,
                settled_by: None,
            }],
            sessions: vec![UpdateSession {
                session_id: WEB.into(),
                name: "acme-web".into(),
                purpose: "Ship the release".into(),
                text: "A long paragraph about where the release is and what comes next.".into(),
                refs: vec![],
            }],
            kept: vec!["I kept your order: the release waits for CI.".into()],
            footer: Some("Notes today: 90 of 100 cheap calls.".into()),
            more_items: 1,
            ..Default::default()
        }
    }

    fn texts(blocks: &[Value]) -> Vec<String> {
        blocks
            .iter()
            .flat_map(|b| {
                let mut t: Vec<String> = Vec::new();
                if let Some(s) = b["text"]["text"].as_str() {
                    t.push(s.to_string());
                }
                for e in b["elements"].as_array().into_iter().flatten() {
                    if let Some(s) = e["text"].as_str() {
                        t.push(s.to_string());
                    }
                }
                t
            })
            .collect()
    }

    fn lines(blocks: &[Value]) -> Vec<String> {
        texts(blocks)
            .iter()
            .flat_map(|t| t.lines().map(str::to_string).collect::<Vec<_>>())
            .collect()
    }

    fn no_buttons(blocks: &[Value]) {
        let raw = serde_json::to_string(blocks).unwrap();
        assert!(!raw.contains("\"actions\""), "{raw}");
        assert!(!raw.contains("\"button\""), "{raw}");
        assert!(!raw.contains("\"accessory\""), "{raw}");
    }

    #[test]
    fn a_typical_update_over_three_sessions_is_one_line_an_item_grouped_by_session() {
        let u = shown(&typical(), &|_| false);
        let (fallback, blocks) = slack_blocks(&u, &|_| Some("https://desk.example/items/x".into()));
        let lines = lines(&blocks);
        assert_eq!(
            lines,
            [
                "*Unblocked web's release; one thing for you.*",
                "*acme-docs*",
                "● For you: Rotate the staging deploy key in your vault — the old one leaked · <https://desk.example/items/x|Done>",
                "*acme-web*",
                "✓ Lifted my hold on tonight's release — CI is green again",
                "*acme-api*",
                "✓ Restarted the stuck test run",
                "● Needs you: Drop the orders table or migrate it?",
                "+1 more in Desk",
            ],
        );
        assert_eq!(lines.len(), 9);
        assert_eq!(fallback, "Unblocked web's release; one thing for you.");
        // At Act fully: no buttons, no answers, no recommendation, and
        // nothing past the critical facts (paragraphs, kept, footer, left
        // alone, details).
        no_buttons(&blocks);
        let all = texts(&blocks).join("\n");
        for gone in [
            "Migrate",
            "fixtures",
            "long paragraph",
            "kept your order",
            "cheap calls",
            "Left alone",
            "behind more",
            WEB,
        ] {
            assert!(!all.contains(gone), "{gone} in {all}");
        }
    }

    #[test]
    fn what_was_settled_collapses_and_a_session_the_person_is_in_is_not_said() {
        let mut u = typical();
        u.brief[1].items[0].settled_by = Some(SettledBy::Session);
        let u = shown(&u, &|s| s == API);
        let (_, blocks) = slack_blocks(&u, &|_| None);
        let all = texts(&blocks).join("\n");
        assert!(!all.contains("acme-web"), "{all}");
        assert!(!all.contains("acme-api"), "{all}");
        assert!(!all.contains("orders table"), "{all}");
        // The headline that named web's release is said again from what
        // is, and what was left out (maybe about api) is not counted.
        assert_eq!(u.headline, "1 for you.");
        assert_eq!(
            lines(&blocks),
            [
                "*1 for you.*",
                "*acme-docs*",
                "● For you: Rotate the staging deploy key in your vault — the old one leaked",
            ]
        );
        no_buttons(&blocks);
        // Everything about sessions the person is in: nothing to say.
        let none = shown(&typical(), &|_| true);
        assert!(!says_anything(&MentionedUpdate {
            kept: vec![],
            ..none.clone()
        }));
        assert!(hides(&typical(), &none));
        assert!(!hides(&typical(), &shown(&typical(), &|_| false)));
    }

    #[test]
    fn what_slack_shows_is_escaped() {
        let mut u = typical();
        u.headline = "Web <b> & `code`".into();
        u.brief[1].label = "*web* <@U1>".into();
        u.brief[1].items[0].action = "Pinged <!here> & *everyone* on deploy_key".into();
        u.brief[1].items[0].why = Some("_urgent_ <x|y>".into());
        u.needs_you[0].question = "Drop <#C1>?".into();
        let (_, blocks) = slack_blocks(&shown(&u, &|_| false), &|_| None);
        let all = texts(&blocks).join("\n");
        assert!(all.contains("*Web &lt;b&gt; &amp; code*"), "{all}");
        assert!(all.contains("*web &lt;@U1&gt;*"), "{all}");
        assert!(
            all.contains(
                "✓ Pinged &lt;!here&gt; &amp; everyone on deploy_key — urgent &lt;x|y&gt;"
            ),
            "{all}"
        );
        assert!(all.contains("● Needs you: Drop &lt;#C1&gt;?"), "{all}");
    }

    #[test]
    fn the_floor_says_purpose_and_state_plainly_without_ids() {
        let (purpose, state) = floor_line(&shown(&typical(), &|_| false), None);
        assert_eq!(purpose, "Keeping you posted on the crew.");
        assert_eq!(state, "Unblocked web's release; one thing for you.");
        let mut u = typical();
        u.headline = "Need you on web?".into();
        let (purpose, state) = floor_line(&u, Some(""));
        assert_eq!(purpose, "Catching you up on the crew.");
        assert_eq!(state, "Need you on web.");
        let (_, state) = floor_line(&u, Some("Nothing needs you. acme-web finished."));
        assert_eq!(state, "Nothing needs you. acme-web finished.");
        // A session the person is in: the line says only what is shown.
        let (_, state) = floor_line(&shown(&typical(), &|s| s == WEB || s == API), None);
        assert_eq!(state, "1 for you.");
        for id in [WEB, API, DOCS] {
            assert!(!state.contains(id));
        }
        // Only a closing question mark goes; a link's stays.
        u.headline = "See https://x.example/?a=1 now?".into();
        assert_eq!(floor_line(&u, None).1, "See https://x.example/?a=1 now.");
        u.headline = "Web moved on…".into();
        assert_eq!(floor_line(&u, None).1, "Web moved on…");
    }

    #[test]
    fn a_free_form_reply_is_read_into_the_brief_and_held_to_its_lengths() {
        let reply = "Web's release is unblocked; one thing for you.\n\
            \n\
            [acme-web]\n\
            ✓ Lifted my hold on tonight's release because the pipeline went green after the flaky test was fixed — CI is green again after the flaky integration test fix landed\n\
            I suggest we also bump the version next.\n\
            [acme-api]\n\
            - ✓ Restarted the stuck test run.\n\
            [acme-docs]\n\
            For you: Rotate the staging deploy key in your vault — the old one leaked\n\
            ✓ Rebuilt the docs site — stale\n\
            Should I also tag the release?";
        let u = parse_reply(reply, Utc::now());
        assert_eq!(u.headline, "Web's release is unblocked; one thing for you.");
        assert_eq!(u.more_items, 1);
        let (_, blocks) = slack_blocks(&u, &|_| None);
        let lines = lines(&blocks);
        assert_eq!(
            lines,
            [
                "*Web's release is unblocked; one thing for you.*",
                "*acme-docs*",
                "● For you: Rotate the staging deploy key in your vault — the old one leaked",
                "*acme-web*",
                "✓ Lifted my hold on tonight's release because the pipeline went green… — CI is green again after the flaky integration…",
                "*acme-api*",
                "✓ Restarted the stuck test run",
                "+1 more in Desk",
            ]
        );
        no_buttons(&blocks);
        let all = lines.join("\n");
        assert!(!all.contains("suggest"), "{all}");
        assert!(!all.contains("tag the release"), "{all}");
    }

    #[test]
    fn what_reads_like_a_mark_but_is_the_lines_own_is_kept() {
        let reply = "[WIP] Release is unblocked\n\
            - [x] ✓ -5% latency on the api — cache warmed\n\
            [acme-web] ✓ 3.5x faster builds - sccache on\n\
            ✓ Suggested fix merged — review passed\n\
            ✓ I can rerun it later\n\
            For you the build passed\n\
            [PR 12](https://x.example) merged";
        let u = parse_reply(reply, Utc::now());
        assert_eq!(u.headline, "[WIP] Release is unblocked");
        let all: Vec<(String, String)> = u
            .brief
            .iter()
            .flat_map(|g| g.items.iter().map(|i| (g.label.clone(), i.action.clone())))
            .collect();
        assert_eq!(
            all,
            [
                ("Crew".to_string(), "-5% latency on the api".to_string()),
                ("acme-web".to_string(), "3.5x faster builds".to_string()),
                ("acme-web".to_string(), "Suggested fix merged".to_string()),
            ]
        );
        assert_eq!(u.more_items, 0);
    }

    #[test]
    fn hiding_only_a_sessions_paragraph_is_hiding() {
        let u = typical();
        let s = shown(&u, &|_| false);
        assert!(!hides(&u, &s));
        let mut only = typical();
        only.brief.clear();
        only.needs_you.clear();
        let s = shown(&only, &|s| s == WEB);
        assert!(hides(&only, &s));
        assert!(!says_anything(&MentionedUpdate { kept: vec![], ..s }));
    }

    #[test]
    fn a_reply_out_of_form_is_its_first_sentence() {
        let u = parse_reply(
            "I looked at everything and the web release is now moving again after I lifted the hold. Next I will watch CI.",
            Utc::now(),
        );
        assert!(u.brief.is_empty());
        assert_eq!(
            u.headline,
            "I looked at everything and the web release is now moving again…"
        );
        let (_, blocks) = slack_blocks(&u, &|_| None);
        assert_eq!(blocks.len(), 1);
    }
}
