//! Rich blocks in chat: the `wf-table` / `wf-chart` / `wf-form` / `wf-map` /
//! `wf-diagram` / `wf-image` fenced JSON blocks a client renders as
//! interactive components (the shared `@workforce/rich` renderer).
//!
//! A block is taught to the model only for the features both the agent has
//! on (`[rich]` in its `config.toml`) and the client says it renders, so the
//! model never learns a syntax its reader would see as raw JSON. The text
//! describes exactly what the renderer's validator accepts, with few-shot
//! examples, which measurably raise spec validity.

use serde::{Deserialize, Serialize};

/// Which rich blocks are in play, named as the renderer's own flags. The
/// same shape is an agent's `[rich]` table, a client's declaration of what
/// it renders, and the two intersected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RichFeatures {
    pub tables: bool,
    pub charts: bool,
    pub forms: bool,
    pub maps: bool,
    pub diagrams: bool,
    pub images: bool,
    /// The reader draws a chart the `chart` tool made from its spec, shown
    /// in place of the picture. Only a client declares it: it is not a
    /// fence, so it is never taught, and an agent's `[rich]` cannot set it.
    #[serde(skip)]
    pub chart_specs: bool,
}

impl RichFeatures {
    /// Every feature on.
    pub const ALL: Self = Self {
        tables: true,
        charts: true,
        forms: true,
        maps: true,
        diagrams: true,
        images: true,
        chart_specs: false,
    };

    /// The features named in `names` — flag names such as `tables`, as a
    /// comma-separated list or one by one. A name this hub does not know is
    /// ignored, so a newer client still talks to an older hub.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut features = Self::default();
        for name in names.into_iter().flat_map(|n| n.split(',')) {
            if let Some(flag) = features.flag_mut(name.trim()) {
                *flag = true;
            }
        }
        features
    }

    /// The flag names of the features that are on, in a fixed order.
    pub fn names(self) -> Vec<&'static str> {
        self.flags()
            .into_iter()
            .filter_map(|(name, on)| on.then_some(name))
            .collect()
    }

    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    /// The features on in both.
    pub fn intersect(self, other: Self) -> Self {
        Self {
            tables: self.tables && other.tables,
            charts: self.charts && other.charts,
            forms: self.forms && other.forms,
            maps: self.maps && other.maps,
            diagrams: self.diagrams && other.diagrams,
            images: self.images && other.images,
            chart_specs: self.chart_specs && other.chart_specs,
        }
    }

    /// The system-prompt block teaching these features, or `None` when they
    /// are all off.
    pub fn system_block(self) -> Option<String> {
        let fences = Self {
            chart_specs: false,
            ..self
        };
        if fences.is_empty() {
            return None;
        }
        let mut block = String::from(HEADER);
        for (on, section) in [
            (self.tables, TABLES),
            (self.charts, CHARTS),
            (self.forms, FORMS),
            (self.maps, MAPS),
            (self.diagrams, DIAGRAMS),
            (self.images, IMAGES),
        ] {
            if on {
                block.push_str(section);
            }
        }
        Some(block)
    }

    fn flags(self) -> [(&'static str, bool); 7] {
        [
            ("tables", self.tables),
            ("charts", self.charts),
            ("forms", self.forms),
            ("maps", self.maps),
            ("diagrams", self.diagrams),
            ("images", self.images),
            ("chart_specs", self.chart_specs),
        ]
    }

    fn flag_mut(&mut self, name: &str) -> Option<&mut bool> {
        match name {
            "tables" => Some(&mut self.tables),
            "charts" => Some(&mut self.charts),
            "forms" => Some(&mut self.forms),
            "maps" => Some(&mut self.maps),
            "diagrams" => Some(&mut self.diagrams),
            "images" => Some(&mut self.images),
            "chart_specs" => Some(&mut self.chart_specs),
            _ => None,
        }
    }
}

/// The fenced examples a block teaches, as `(fence language, JSON text)`.
pub fn fenced_examples(block: &str) -> impl Iterator<Item = (&str, &str)> {
    block.split("```").skip(1).step_by(2).filter_map(|fence| {
        let (lang, body) = fence.split_once('\n')?;
        lang.starts_with("wf-").then(|| (lang, body.trim()))
    })
}

const HEADER: &str = "\n\n## Rich output\n\
    When you present structured data, use the special fenced blocks below. Each block \
    must contain ONLY strict JSON — double-quoted keys and strings, no trailing commas, \
    no comments. Numbers must be JSON numbers, not strings — the chat adds thousands \
    separators and sensible decimals when it displays them. In prose and plain markdown \
    tables, write numbers the way a person reads them: `523,797.33`, not `523797.33`. \
    Never invent or pad data. Add a one-sentence takeaway near the block, and do not \
    repeat the full data in prose.";

const TABLES: &str = "\n\n### Data tables\n\
    For any tabular result (lists of records, comparisons, breakdowns) of 3 or more rows, \
    emit a `wf-table` block:\n\n\
    ```wf-table\n\
    {\"title\":\"Q3 invoices\",\"columns\":[\"Customer\",\
    {\"key\":\"invoiced\",\"label\":\"Invoiced\",\"format\":\"currency\",\"unit\":\"£\"},\"Due date\"],\
    \"rows\":[[\"Acme Ltd\",12800.5,\"2026-07-01\"],[\"Birch & Co\",9400,\"2026-07-14\"],\
    [\"Cedar Partners\",6250,\"2026-07-21\"]]}\n\
    ```\n\n\
    - `columns`: an array of names, or objects `{\"key\":\"...\",\"label\":\"...\",\
    \"align\":\"left|right\",\"format\":\"...\",\"unit\":\"£\"}`.\n\
    - `rows`: arrays aligned with `columns`. Cells are strings, numbers, booleans, or null.\n\
    - Emit dates as ISO `YYYY-MM-DD` strings and money as plain numbers.\n\
    - Numeric cells display with thousands separators and sensible decimals. Set `format` \
    when the column name alone would mislead: `currency` (always two decimals, prefixed \
    with `unit`) for money, `percent` for percentages, `integer` for counts, `id` for \
    references, codes and years that must never be grouped, `number` for any other measure.\n\
    - For a LARGE result staged out of context (you were handed an `artifact_id` for \
    a spilled tool result or a `data.query` result), reference it instead of inlining \
    the rows — the chat loads, sorts, filters and pages the whole dataset. You may add \
    `columns` objects to relabel or format its columns by key:\n\n\
    ```wf-table\n\
    {\"title\":\"Recent leads\",\"artifact_id\":\"art_0123456789abcdef0123456789abcdef\",\
    \"columns\":[{\"key\":\"total_fee\",\"label\":\"Fee\",\"format\":\"currency\",\"unit\":\"£\"}]}\n\
    ```\n\
    Never paste a staged result's preview rows into an inline table — reference the \
    artifact so the user gets the full, browsable table.";

const CHARTS: &str = "\n\n### Charts\n\
    When a trend, share, or comparison is clearer as a picture, emit a `wf-chart` block. \
    `kind` is one of `pie`, `bar`, `line`, `area`, `scatter`:\n\n\
    ```wf-chart\n\
    {\"kind\":\"bar\",\"title\":\"Signups by plan\",\"labels\":[\"Free\",\"Pro\",\"Team\"],\
    \"series\":[{\"name\":\"March\",\"data\":[420,180,60]},{\"name\":\"April\",\"data\":[510,220,90]}]}\n\
    ```\n\n\
    ```wf-chart\n\
    {\"kind\":\"pie\",\"title\":\"Ticket status\",\"labels\":[\"Open\",\"Pending\",\"Closed\"],\
    \"series\":[{\"data\":[14,8,52]}]}\n\
    ```\n\n\
    - `labels`: category names (x-axis, or pie slice names). `series[].data`: numbers \
    aligned with `labels`; for `scatter`, `[x,y]` number pairs instead.\n\
    - Optional: `\"stacked\":true` (bar/line/area), `\"horizontal\":true` (bar), \
    `\"x_label\"`, `\"y_label\"`, `\"unit\"` (e.g. `\"£\"`, shown in tooltips).\n\
    - One series per pie, at most 8 series per chart. Prefer a table when there are \
    more than ~8 categories.";

const FORMS: &str = "\n\n### Forms\n\
    When you need the user to supply structured input before you can act (create a \
    record, update fields), emit a `wf-form` block. They fill it in and their exact values \
    return to you next turn as the arguments for that action:\n\n\
    ```wf-form\n\
    {\"title\":\"New lead\",\"submit_label\":\"Create lead\",\"note\":\"We'll create this in the CRM.\",\
    \"fields\":[\
    {\"name\":\"company\",\"label\":\"Company\",\"type\":\"text\",\"required\":true,\"max_length\":120},\
    {\"name\":\"email\",\"label\":\"Email\",\"type\":\"email\",\"required\":true},\
    {\"name\":\"value\",\"label\":\"Deal value (£)\",\"type\":\"number\",\"min\":0},\
    {\"name\":\"status\",\"label\":\"Status\",\"type\":\"select\",\"required\":true,\
    \"options\":[{\"value\":\"open\",\"label\":\"Open\"},{\"value\":\"won\",\"label\":\"Won\"},\
    {\"value\":\"lost\",\"label\":\"Lost\"}],\"value\":\"open\"},\
    {\"name\":\"owner\",\"label\":\"Owner\",\"type\":\"select\",\"required\":true,\
    \"options_ref\":{\"artifact_id\":\"art_0123456789abcdef0123456789abcdef\",\"value\":\"id\",\"label\":\"name\"}}\
    ]}\n\
    ```\n\n\
    - Every field has `name` (the unique tool-argument key) and `type`, plus optional `label` \
    (defaults to `name`), `required` (default false), `help` (one line under the control), \
    `value` (prefill), and `error` (a message shown under the field).\n\
    - Types: `text`/`textarea` (`placeholder`, `min_length`, `max_length`; `pattern` on \
    `text`, `rows` on `textarea`); `email`; `number` (`min`, `max`, `step`); `date` (ISO \
    `YYYY-MM-DD`, `min`, `max`); `select` (`options` or `options_ref`; add `multiple` to \
    `options` for an array value); `checkbox` (no `options` is a single boolean, with \
    `options` a multi-select); `radio` (`options` required); `hidden` (a `value` carried \
    straight through). An option is `{\"value\":\"id\",\"label\":\"Name\"}` — label defaults \
    to value.\n\
    - Resolve options BEFORE emitting the form. A fixed enum you already hold: inline it in \
    `options` (only for small sets, roughly 50 or fewer). A dynamic list (owners, accounts, \
    pipelines): call the relevant list tool, then inline it if small, or — if it is large — \
    stage it as a tabular artifact and reference it with `options_ref` (its `artifact_id` plus \
    the `value` and `label` column names). Never inline a large list; it overflows the reply and \
    truncates the block.\n\
    - Prefill `value` with sensible defaults, or the record's current values when editing.\n\
    - When the user submits, take the action with the exact values they returned. If a tool \
    REJECTS a value, re-emit the SAME `wf-form` with every field's `value` preserved and a \
    per-field `error` on the ones at fault, so they fix only what is wrong — never make them \
    re-enter the whole form.\n\
    - Never put secrets or fields the user cannot see in a `hidden` field.";

const MAPS: &str = "\n\n### Maps\n\
    When geography itself is the point — a metric that varies by region and reads best \
    as a shaded map — emit a `wf-map` choropleth. Only when the geography matters: \
    otherwise keep using a chart or table, and never map data that has no geography:\n\n\
    ```wf-map\n\
    {\"kind\":\"choropleth\",\"title\":\"January 2026 leads by postcode area\",\
    \"geo\":\"uk-postcode-areas\",\"value_label\":\"Leads\",\"unit\":\"\",\
    \"regions\":[{\"name\":\"W\",\"value\":489},{\"name\":\"SW\",\"value\":451},\
    {\"name\":\"EC\",\"value\":92}]}\n\
    ```\n\n\
    - `kind` is always `\"choropleth\"`. `regions`: one `{\"name\":…,\"value\":…}` per \
    region, values plain numbers. `geo` names the geography and fixes how you spell \
    each region:\n\
    - `geo`:`\"uk-postcode-areas\"` — `name` is the UK postcode AREA code: a postcode's \
    leading letters with the district digit(s) and the rest removed, uppercase \
    (`SW1A 1AA` → `\"SW\"`, `EC2R` → `\"EC\"`, `M1` → `\"M\"`, `LS6` → `\"LS\"`).\n\
    - `geo`:`\"world-countries\"` — `name` is the country's English name as Natural \
    Earth spells it (`\"United Kingdom\"`, `\"United States of America\"`, `\"France\"`, \
    `\"Czechia\"`).\n\
    - Optional: `\"value_label\"` (legend/tooltip label), `\"unit\"` (tooltip prefix, \
    e.g. `\"£\"`), and `\"min\"`/`\"max\"`. For a non-count metric with a narrow range \
    (e.g. revenue 200k–500k), set `min`/`max` so the scale spans the data rather than \
    0→max.\n\
    - Use the exact codes/names above; unmatched regions are dropped with a visible \
    notice, so never invent them. You may pair a table of the same data for an exact, \
    accessible view.";

const DIAGRAMS: &str = "\n\n### Diagrams\n\
    When the answer is a STRUCTURE — a process, a decision path, a hand-off between \
    people or systems, or an ordering of events — emit a `wf-diagram` block. Describe \
    it in meaning only: name the boxes and what connects to what, never positions or \
    coordinates. `kind` is `flow` or `sequence`:\n\n\
    ```wf-diagram\n\
    {\"kind\":\"flow\",\"title\":\"Order pipeline\",\"direction\":\"down\",\
    \"nodes\":[{\"id\":\"in\",\"label\":\"Order received\",\"shape\":\"round\"},\
    {\"id\":\"chk\",\"label\":\"Stock available?\",\"shape\":\"diamond\"},\
    {\"id\":\"ship\",\"label\":\"Pick and ship\",\"group\":\"Warehouse\"},\
    {\"id\":\"bo\",\"label\":\"Backorder\",\"group\":\"Warehouse\",\"tone\":\"warn\"}],\
    \"edges\":[{\"from\":\"in\",\"to\":\"chk\"},\
    {\"from\":\"chk\",\"to\":\"ship\",\"label\":\"yes\"},\
    {\"from\":\"chk\",\"to\":\"bo\",\"label\":\"no\"}]}\n\
    ```\n\n\
    ```wf-diagram\n\
    {\"kind\":\"sequence\",\"title\":\"Refund request\",\
    \"participants\":[{\"id\":\"c\",\"label\":\"Customer\"},{\"id\":\"a\",\"label\":\"Agent\"},\
    {\"id\":\"p\",\"label\":\"Payments\"}],\
    \"steps\":[{\"from\":\"c\",\"to\":\"a\",\"label\":\"Requests refund\"},\
    {\"from\":\"a\",\"to\":\"p\",\"label\":\"Check eligibility\"},\
    {\"from\":\"p\",\"to\":\"a\",\"label\":\"Eligible\",\"style\":\"dashed\"},\
    {\"from\":\"a\",\"to\":\"c\",\"label\":\"Confirms refund\"}]}\n\
    ```\n\n\
    - Use a diagram only for structure: steps, decisions, connections, ordering. Never \
    for quantities, never for a list of records, and never for something with no \
    structure to show.\n\
    - KEEP IT SMALL: aim for about 12 nodes and never more than 20 — it is read in a \
    narrow panel, and beyond that a table is clearer. Labels of at most 6 words.\n\
    - `flow`: every node needs a unique short `id` and a `label`; every edge's `from` \
    and `to` must name a declared node (unmatched ones are dropped with a visible \
    notice, so never invent ids). Optional per edge: `label`, `\"style\":\"dashed\"`, \
    `\"arrow\":\"both\"` or `\"none\"`.\n\
    - `shape`: `round` for a start or end, `diamond` for a decision (label it as a \
    question and label its outgoing edges with the answers), `cylinder` for a store, \
    `box` otherwise. `tone` (`accent`, `good`, `warn`, `bad`) only to mark an \
    exception or failure path — never to decorate.\n\
    - `group` bands together nodes owned by the same team or system, and each \
    group is given its own colour automatically — so grouping is worth doing \
    whenever a process crosses more than one team. Never ask for colours \
    yourself; there is no colour field.\n\
    - `\"direction\":\"down\"` (the default) suits a narrow panel; use `\"right\"` only \
    for a short chain.\n\
    - `sequence`: 2 to 12 `participants` with unique ids, then `steps` in time order. \
    `\"style\":\"dashed\"` reads as a reply, `from` equal to `to` is a self-step, and a \
    step may carry a `note` shown beneath it.\n\
    - Leave the visual style alone unless the user asks for a hand-drawn or \
    whiteboard look, then add `\"style\":\"sketch\"`.\n\
    - Add a one-sentence takeaway near the block and do not restate the whole diagram \
    in prose.";

const IMAGES: &str = "\n\n### Showing pictures\n\
    You CAN show a picture back to the user. Any file with an `artifact_id` \
    that you have been told about — one they attached earlier in this \
    conversation, or one a tool returned to you — can be put in the reply by \
    emitting a `wf-image` block naming that id:\n\n\
    ```wf-image\n\
    {\"images\":[{\"artifact_id\":\"art_0123456789abcdef0123456789abcdef\",\
    \"caption\":\"the photo you sent\"}]}\n\
    ```\n\n\
    - ALWAYS a list, even for one picture — several images in one block are laid \
    out together, which is what \"show me the photos for this item\" wants. At most \
    12 pictures in a block.\n\
    - Use ONLY an `artifact_id` you have actually been given. Never invent one, \
    never guess at the format, and never put a web address here — an id you made \
    up shows the user a broken picture.\n\
    - `caption` is optional and shown beneath. Add `alt` when the picture needs \
    describing for someone who cannot see it.\n\
    - When the user asks to see something again, show it rather than \
    explaining that you cannot.\n\
    - A picture you know only by a web address cannot go in a `wf-image` block; \
    give the address as a link instead.";

#[cfg(test)]
mod tests {
    use super::*;

    /// Every example in `block` as strict JSON, with its fence language. The
    /// model copies these verbatim, so a malformed one teaches it garbage.
    fn examples(block: &str) -> Vec<(&str, serde_json::Value)> {
        fenced_examples(block)
            .map(|(lang, json)| {
                let parsed = serde_json::from_str(json)
                    .unwrap_or_else(|e| panic!("example must be strict JSON: {e}\n{json}"));
                (lang, parsed)
            })
            .collect()
    }

    fn only(name: &str) -> RichFeatures {
        RichFeatures::from_names([name])
    }

    const FENCES: [(&str, &str); 6] = [
        ("tables", "wf-table"),
        ("charts", "wf-chart"),
        ("forms", "wf-form"),
        ("maps", "wf-map"),
        ("diagrams", "wf-diagram"),
        ("images", "wf-image"),
    ];

    #[test]
    fn no_features_teach_nothing() {
        assert_eq!(RichFeatures::default().system_block(), None);
    }

    #[test]
    fn each_feature_teaches_its_own_fence_and_no_other() {
        for (feature, fence) in FENCES {
            let block = only(feature).system_block().unwrap();
            assert_eq!(block.matches("## Rich output").count(), 1, "{feature}");
            for (_, other) in FENCES.iter().filter(|(f, _)| *f != feature) {
                assert!(!block.contains(other), "{feature} teaches {other}");
            }
            let examples = examples(&block);
            assert!(!examples.is_empty(), "{feature} has an example");
            assert!(examples.iter().all(|(lang, _)| *lang == fence), "{feature}");
        }
    }

    #[test]
    fn all_features_share_one_header() {
        let block = RichFeatures::ALL.system_block().unwrap();
        assert_eq!(block.matches("## Rich output").count(), 1);
        for (_, fence) in FENCES {
            assert!(block.contains(fence), "{fence}");
        }
        assert_eq!(examples(&block).len(), 9);
    }

    /// Both diagram kinds are taught: a `flow`-only block would leave the
    /// model no way to express an interaction over time.
    #[test]
    fn diagrams_teach_both_kinds() {
        let kinds: Vec<_> = examples(&only("diagrams").system_block().unwrap())
            .into_iter()
            .map(|(_, json)| json["kind"].clone())
            .collect();
        assert_eq!(kinds, ["flow", "sequence"]);
    }

    /// A `wf-image` names an artifact by id: a model that invents one shows a
    /// broken picture, and a URL is never a valid id.
    #[test]
    fn images_forbid_invented_ids_and_urls() {
        let block = only("images").system_block().unwrap();
        assert!(block.contains("Never invent one"));
        assert!(block.contains("never put a web address here"));
        assert!(block.contains(r#""images":["#));
    }

    /// Every artifact id an example names has the shape the renderer accepts,
    /// `art_` and 32 lowercase hex digits; a different shape degrades the
    /// block to its JSON.
    #[test]
    fn example_artifact_ids_have_the_shape_the_renderer_accepts() {
        let block = RichFeatures::ALL.system_block().unwrap();
        let ids: Vec<&str> = block
            .match_indices("\"artifact_id\":\"")
            .map(|(at, prefix)| {
                let id = &block[at + prefix.len()..];
                &id[..id.find('"').unwrap()]
            })
            .collect();
        assert_eq!(ids.len(), 3);
        for id in ids {
            let hex = id.strip_prefix("art_").expect("art_ prefix");
            assert_eq!(hex.len(), 32, "{id}");
            assert!(hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
    }

    #[test]
    fn names_parse_one_by_one_or_comma_separated_and_unknown_ones_are_ignored() {
        let declared = RichFeatures::from_names(["tables, charts", "video", "", "images"]);
        assert_eq!(
            declared,
            RichFeatures {
                tables: true,
                charts: true,
                images: true,
                ..Default::default()
            }
        );
        assert_eq!(declared.names(), ["tables", "charts", "images"]);
        assert_eq!(RichFeatures::from_names(declared.names()), declared);
        assert!(RichFeatures::from_names(["video,audio"]).is_empty());
    }

    #[test]
    fn intersect_keeps_what_both_have_on() {
        let agent = RichFeatures::from_names(["tables,charts,forms"]);
        let client = RichFeatures::from_names(["charts,forms,maps"]);
        assert_eq!(
            agent.intersect(client).names(),
            ["charts", "forms"],
            "the agent's maps and the client's tables are not both on"
        );
        assert!(agent.intersect(RichFeatures::default()).is_empty());
        assert_eq!(RichFeatures::ALL.intersect(client), client);
    }

    /// A reader that draws chart specs says so with the blocks it renders;
    /// the session keeps it, but it teaches nothing and no agent can set it.
    #[test]
    fn chart_specs_is_a_readers_flag_never_a_fence() {
        let desk = RichFeatures::from_names(["tables,charts,chart_specs"]);
        assert!(desk.chart_specs && desk.charts);
        assert_eq!(desk.names(), ["tables", "charts", "chart_specs"]);
        assert_eq!(RichFeatures::from_names(desk.names()), desk);
        assert_eq!(only("chart_specs").system_block(), None, "nothing to teach");
        assert_eq!(
            desk.system_block(),
            RichFeatures::from_names(["tables,charts"]).system_block()
        );
        let config: RichFeatures = toml::from_str("chart_specs = true\ncharts = true").unwrap();
        assert!(!config.chart_specs && config.charts);
        assert!(
            !RichFeatures::ALL.intersect(desk).chart_specs,
            "an agent with every block on still never has it"
        );
    }

    #[test]
    fn the_config_table_defaults_every_flag_off() {
        let config: RichFeatures = toml::from_str("charts = true").unwrap();
        assert_eq!(config.names(), ["charts"]);
        assert!(toml::from_str::<RichFeatures>("").unwrap().is_empty());
    }
}
