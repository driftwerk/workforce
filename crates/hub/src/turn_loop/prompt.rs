//! What the prompt says: the notes, skill lists, footers, history and
//! voice directives a turn's prompt is rendered from.

/// Per-turn prompt note describing gated MCP workspaces and their active variant
/// for this session. `None` when no workspaces exist. Tells the agent which
/// variant each workspace's tools hit, loudly flags a non-default (e.g.
/// staging) override, and makes clear it can OFFER a dormant variant but cannot
/// switch — only a human's `cmd: use <ws> <variant>` does that.
pub(crate) fn render_mcp_workspace_note(
    overrides: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let views = workforce_agent::llm::mcp_workspace::workspace_summary_with(overrides);
    if views.is_empty() {
        return None;
    }
    let mut s = String::from(
        "\n\n## MCP workspaces (variant-gated)\n\
         Some MCP servers are grouped into workspaces where exactly ONE \
         variant (e.g. staging vs prod) is reachable at a time. You CANNOT switch \
         variants yourself — only a human can, by typing `cmd: use <workspace> <variant>`. \
         You may offer to check another variant, but never claim data from \
         one variant is from another. Before answering why a server isn't \
         reachable, call `env.status` for ground truth (it shows which servers \
         are connected, installed-but-unauthorised, or absent, with the fix) — \
         do not guess, claim a server was uninstalled, or offer to edit the \
         database.\n",
    );
    for v in &views {
        match &v.active_env {
            Some(variant) => {
                let overridden = overrides
                    .get(&v.workspace)
                    .map(|e| e == variant)
                    .unwrap_or(false);
                if overridden {
                    s.push_str(&format!(
                        "\n⚠️ `{}` is currently pointed at **{}** (a session override, NOT the \
                         default). Treat all `{}` results as **{}** data.\n",
                        v.workspace, variant, v.workspace, variant
                    ));
                } else {
                    s.push_str(&format!(
                        "\n`{}` is at its default variant **{}**.\n",
                        v.workspace, variant
                    ));
                }
            }
            None => {
                s.push_str(&format!(
                    "\n`{}` has NO active variant (no default set) — its tools are all \
                     dormant until a human runs `cmd: use {} <variant>`.\n",
                    v.workspace, v.workspace
                ));
            }
        }
        let dormant: Vec<&str> = v
            .members
            .iter()
            .filter(|m| !m.active)
            .map(|m| m.env.as_str())
            .collect();
        if !dormant.is_empty() {
            s.push_str(&format!(
                "    Dormant (ask a human to run `cmd: use {} <variant>`): {}.\n",
                v.workspace,
                dormant.join(", ")
            ));
        }
    }
    Some(s)
}

/// Render the always-visible pinned block, dating every entry.
///
/// Pinned memories used to carry no time at all, which made them read as
/// timeless truths. Even a standing instruction has a date worth knowing: rules
/// are given in a context that changes, and when two pins contradict each other
/// the later one is almost always the one meant. Without a date there is
/// nothing to decide that on, and nothing to notice a rule has been overtaken.
///
/// The date is absolute rather than relative because these persist for months —
/// "last week" ages badly in a block that is re-read every day, and an absolute
/// date can be compared against the current date the prompt already carries.
pub(crate) fn format_pinned_memories(
    memories: &[workforce_memory::types::Memory],
    agent_id: &workforce_core::types::AgentId,
) -> String {
    let formatted: String = memories
        .iter()
        .map(|m| {
            let when = m.created_at.format("%Y-%m-%d");
            match m.origin_label(agent_id) {
                Some(label) => format!("- (pinned {when}, {label}) {}", m.content),
                None => format!("- (pinned {when}) {}", m.content),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if formatted.len() > 2000 {
        format!("{}...", &formatted[..formatted.floor_char_boundary(2000)])
    } else {
        formatted
    }
}

/// Decide whether to inject the welcome directive + prior Slack-thread
/// history into a session's first message. Only fires for genuinely-new
/// user-conversation sessions anchored to a Slack thread; internal channels
/// (workflow steps, schedulers, subagents, consolidation,
/// knowledge-maintenance) share the originating thread for status output
/// but are not continuations of the user conversation and must not pick up
/// its history.
pub(crate) fn should_inject_session_intro(
    channel: &str,
    is_new_session: bool,
    channel_id: Option<&str>,
    thread_id: Option<&str>,
) -> bool {
    let private = channel_id
        .map(|c| {
            workforce_common::channels::ConversationVisibility::from_slack_channel_id(c)
                .is_private()
        })
        .unwrap_or(false);
    is_new_session
        && channel_id.is_some()
        && thread_id.is_some()
        && private
        && !crate::turn_extraction::is_internal_channel(channel)
}

/// Classify a session's conversational context from the inbound channel/thread,
/// reading the channel id directly so it works at session-load time (before the
/// per-turn task-locals are scoped). Private (a 1:1 DM, including a CLI surface
/// with no channel id) is the only context relationship tracking attributes to
/// a single human.
pub(crate) fn classify_context_type(
    channel_id: Option<&str>,
    thread_id: Option<&str>,
) -> workforce_common::session::SessionContextType {
    use workforce_common::channels::ConversationVisibility;
    use workforce_common::session::SessionContextType;
    let private = channel_id
        .filter(|s| !s.is_empty())
        .map(ConversationVisibility::from_slack_channel_id)
        .unwrap_or(ConversationVisibility::Private)
        .is_private();
    if private {
        SessionContextType::Dm
    } else if thread_id.is_some() {
        SessionContextType::Thread
    } else {
        SessionContextType::Group
    }
}

/// Whether answer-ready code-context injection may run for this turn.
/// Spoken turns never qualify: spoken small talk brushes symbol names
/// constantly ("what day is it" → a `Day` symbol), and every false positive
/// dumps ~1,300 lines of source into a latency-sensitive prompt.
///
/// Per *turn*, not per session (DW-571). iOS shares one eternal session
/// between speech and typing and only flags the spoken messages, so the
/// session-level `is_voice()` latches on at the first spoken turn and never
/// clears — gating on it silently stripped code context from every typed
/// question thereafter, which is precisely when it is most wanted.
pub(crate) fn code_context_eligible(session: &workforce_common::session::Session) -> bool {
    !session.is_voice_turn()
}

/// Total bytes the post-turn annotation blocks may add to an answer.
///
/// Five independent checks (failed tools, data grounding, code grounding,
/// conclusion review, commitments) each append their own block, and each was
/// individually bounded but collectively unbounded — on a turn where several
/// fired, the annotations ran longer than the answer they annotate and buried
/// it. This is the ceiling on the lot of them.
pub(crate) const FOOTER_TOTAL_MAX_BYTES: usize = 1_600;

/// Assemble the post-turn annotation blocks into the segments to append.
///
/// Blocks are admitted whole, in priority order, until the shared budget is
/// spent. Whole-block admission is deliberate: these blocks exist to say what
/// the reader must double-check, and a mid-sentence truncation deletes exactly
/// that. The highest-priority block is always admitted — a budget that could
/// suppress every warning would be worse than no budget — and anything dropped
/// is named in a trailing line, so a reader is never left thinking the turn was
/// checked clean when it wasn't.
///
/// Shared by the streaming and non-streaming delivery paths so the two cannot
/// drift; the streaming path needs the segments separately to frame each one.
pub(crate) fn footer_segments(blocks: Vec<(&'static str, String)>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut spent = 0usize;
    let mut dropped: Vec<&'static str> = Vec::new();

    for (label, block) in blocks {
        let fits = spent + block.len() <= FOOTER_TOTAL_MAX_BYTES;
        if fits || out.is_empty() {
            spent += block.len();
            out.push(block);
        } else {
            dropped.push(label);
        }
    }

    if !dropped.is_empty() {
        out.push(format!(
            "_{} further check{} flagged something ({}) — not shown here._",
            dropped.len(),
            if dropped.len() == 1 { "" } else { "s" },
            dropped.join(", ")
        ));
    }
    out
}

/// Guidance appended to the system prompt when Slack is connected, so the
/// model emits structural markdown the `rich_text` renderer can turn into a
/// readable, well-spaced layout. The renderer can only space what the model
/// marks up: a single newline is joined into one paragraph, so without blank
/// lines and `##` headings the output collapses into a dense wall of text.
pub(crate) const SLACK_FORMATTING_GUIDANCE: &str = "\n\n## Formatting your Slack replies\n\n\
     Your markdown is rendered into Slack's rich layout, so structure it for a human \
     skimming on a phone:\n\
     - Use `##` headings to label sections — each one starts a new, visually separated \
     block (with a divider above it). Reach for them whenever a reply has more than one part.\n\
     - For a hard separator without a heading, put `---` on its own line with a blank line \
     above and below it; it renders as a full-width divider. Use it sparingly, between major \
     sections only — back-to-back dividers and headings collapse to one.\n\
     - Put a blank line between paragraphs. A single newline does NOT separate them; \
     without the blank line, sentences pile into one dense wall of text.\n\
     - Prefer `-` bullets and `1.` numbered lists over long run-on sentences; one idea \
     per bullet.\n\
     - Keep paragraphs to 1–3 sentences and lead with the answer.\n\
     - Use `**bold**` for key terms and `` `code` `` for IDs, statuses, and values.\n\
     - For links, write `[label](https://…)` or paste the bare URL — never put a URL in \
     backticks, or it won't be clickable.\n\
     - Emoji render from `:shortcode:` (e.g. `:mag:`) or unicode; use them sparingly as \
     section markers, not decoration.";

/// The Available Skills block over `skills`, listing those ready to load, or
/// `None` when none is.
pub(crate) fn render_available_skills(
    skills: &[crate::external_skills::ExternalSkill],
) -> Option<String> {
    let mut available: Vec<_> = skills
        .iter()
        .map(|skill| &skill.discovered)
        .filter(|discovered| discovered.is_available())
        .collect();
    available.sort_by(|a, b| (&a.skill.meta.name, &a.path).cmp(&(&b.skill.meta.name, &b.path)));
    let lines: String = available
        .into_iter()
        .map(|discovered| {
            let meta = &discovered.skill.meta;
            let tools = match meta.tools.is_empty() {
                true => String::new(),
                false => format!("  Tools: {}\n", meta.tools.join(", ")),
            };
            format!(
                "- **{}** (v{}): {}\n{tools}",
                meta.name, meta.version, meta.description
            )
        })
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "\n\n## Available Skills\n\n\
             These skills load on demand to keep your context lean — only a summary \
             is shown here. When a task needs one, call `system.enable_skill('<name>')` \
             to load its full step-by-step procedure, then follow it.\n\n{lines}"
        )
    })
}

/// The catalogue of built-in skills with what the vault says about their
/// credentials. Deliberately takes no runtime state: this block lives in the
/// most-cached prefix and must not move when a skill loads.
pub(crate) fn render_loadable_skills(
    catalog: &[crate::skill_loader::BuiltinSkillMeta],
    ready_by_name: &std::collections::HashMap<String, bool>,
) -> Option<String> {
    let lines: String = catalog
        .iter()
        // `obscura` is opt-in only — its HTTP client truncates large responses
        // to 0 bytes (verified 2026-05-28 against a React SPA: assets/index-*.js
        // returned 200 with no body, page never hydrated). Use `browser.*` for
        // any modern SPA. It stays in the catalog so
        // `system.enable_skill('obscura')` still works on explicit user request,
        // but is not advertised so agents don't auto-discover it.
        .filter(|meta| meta.name != "obscura")
        .map(|meta| {
            let status = match meta.managed_by {
                crate::skill_loader::CredManaged::Oauth => "per-user",
                _ if ready_by_name.get(meta.name).copied().unwrap_or(false) => "ready",
                _ => "not configured",
            };
            format!("- **{}**: {} [{status}]\n", meta.name, meta.description)
        })
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "\n\n## Loadable Skills\n\n\
             The following built-in skills can be activated on demand. \
             Use `system.enable_skill` to load a skill when you need it.\n\n\
             {lines}\n\
             **A skill marked [ready] already has valid credentials.** If it is listed under \
             \"Loaded Skills\" below, its tools are active — call them directly; otherwise call \
             `system.enable_skill` first (it's already configured — this is instant and silent). \
             An active skill's tools may be listed in the tool catalogue instead of your tool \
             list; load them with `tool_load` yourself — that is not a setup step and the user \
             never sees it. NEVER tell the user a \
             [ready] or loaded skill is \"not configured\" / \"not set up\" or walk them through \
             configuration; they have already done it.\n\
             \n**[not configured] and [per-user] are about shared credentials only.** This list \
             is the same for everyone, so it cannot show what the person you are talking to has \
             connected with their own credentials (a [per-user] skill, e.g. google, is only ever \
             connected that way, by each user via OAuth). In a private conversation their own \
             connections are listed under \"Your Connected Skills\" below: a skill listed there \
             is ready for them whatever its badge says here. In a shared channel that section is \
             left out, and its absence tells you nothing. So never tell anyone a \
             [not configured] or [per-user] skill needs setting up on the badge alone: when the \
             task needs it and it is not under \"Your Connected Skills\", call \
             `system.enable_skill('<name>')` (or `system.skill_status('<name>')` to check without \
             loading). Both run as the person you are talking to and pick up their own \
             credentials. Only ask them to add credentials or connect it when that call reports \
             there are none.\n\
             \nWhen adding credentials for a skill that is already loaded, ALWAYS use the \
             multi-workspace vault pattern to avoid overwriting existing credentials. Check \
             `system.skill_status('<skill>')` first."
        )
    })
}

/// Skills active in this process right now that everyone may use, with the
/// hints that are safe to share (shared-scope credentials only). Semi-stable:
/// changes on enable or install, never mid-turn on its own.
///
/// The block sits above the private-context gate and the tracker is
/// process-wide, so a skill one person loaded on their own credentials is
/// left out: its name would tell every other user, shared rooms included,
/// that someone here holds it, and tell their model to call tools that have
/// no credentials for them. That person sees it under "Your Connected
/// Skills".
pub(crate) fn render_loaded_skills(
    loaded: &[crate::skill_state::LoadedSkillState],
    catalog: &[crate::skill_loader::BuiltinSkillMeta],
    shared_ready: &std::collections::HashMap<String, bool>,
) -> Option<String> {
    // A skill with `CredScope::Either`/`PerUser` may have been enabled under
    // one user's scope, baking that user's accounts into the hint; only a
    // hint that is identical for everyone may be rendered here. OAuth skills
    // are per-user by construction and surface under "Your Connected Skills".
    let mut entries: Vec<(
        &crate::skill_state::LoadedSkillState,
        &crate::skill_loader::BuiltinSkillMeta,
    )> = loaded
        .iter()
        .filter_map(|state| {
            catalog
                .iter()
                .find(|meta| meta.name == state.name)
                .map(|meta| (state, meta))
        })
        .filter(|(_, meta)| meta.managed_by != crate::skill_loader::CredManaged::Oauth)
        .filter(|(state, meta)| {
            meta.default_scope == crate::skill_loader::CredScope::Shared
                || shared_ready.get(&state.name).copied().unwrap_or(false)
        })
        .collect();
    entries.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    let lines: String = entries
        .iter()
        .map(|(state, meta)| {
            let hint: String = state
                .context_hint
                .as_deref()
                .filter(|_| meta.default_scope == crate::skill_loader::CredScope::Shared)
                .map(|hint| hint.lines().map(|line| format!("  {line}\n")).collect())
                .unwrap_or_default();
            format!("- **{}**\n{hint}", state.name)
        })
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "\n\n## Loaded Skills\n\n\
             Active in this process — call their tools directly; `system.enable_skill` is not \
             needed and would only reload them.\n\n{lines}"
        )
    })
}

pub(crate) fn format_thread_history(
    messages: &[workforce_common::channels::ChannelMessage],
    user_map: &std::collections::HashMap<String, String>,
) -> String {
    let mut lines = Vec::new();
    for msg in messages {
        let display = user_map
            .get(&msg.user)
            .cloned()
            .or_else(|| msg.user_name.clone())
            .unwrap_or_else(|| msg.user.clone());
        // Render attachments as markers so the agent knows a file exists (and
        // its name, for `view_attachment`) — otherwise a file-only message reads
        // as a blank line, the "ghost in the thread".
        let markers = msg
            .files
            .iter()
            .map(|f| {
                let kind = if f.is_image() { "image" } else { "file" };
                format!("[{kind}: {}]", f.name)
            })
            .collect::<Vec<_>>()
            .join(" ");
        let body = match (msg.text.trim(), markers.as_str()) {
            (text, "") => text.to_string(),
            ("", markers) => markers.to_string(),
            (text, markers) => format!("{text} {markers}"),
        };
        if !body.is_empty() {
            lines.push(format!("{display}: {body}"));
        }
    }
    let result = lines.join("\n\n");
    // Cap at ~20K chars (~5K tokens) to avoid consuming too much context
    if result.len() > 20_000 {
        // Snap to a valid UTF-8 char boundary to avoid panicking on multi-byte chars
        let start = result.ceil_char_boundary(result.len() - 20_000);
        let truncated = &result[start..];
        if let Some(pos) = truncated.find('\n') {
            return format!(
                "[...earlier messages truncated...]\n{}",
                &truncated[pos + 1..]
            );
        }
    }
    result
}

/// `tags` with what was asked only while `recorded`: the request is on the
/// header for the decision export alone, so it follows the switches that
/// govern what the export may read.
pub(crate) fn without_request_unless(
    recorded: bool,
    tags: workforce_agent::trace::TraceTags,
) -> workforce_agent::trace::TraceTags {
    workforce_agent::trace::TraceTags {
        request: tags.request.filter(|_| recorded),
        ..tags
    }
}

/// Where a spoken reply comes out, which changes what the listener can do with
/// it.
///
/// The same words land differently depending on whether they are also written
/// down. On a phone, a name or a number the listener misses is still on screen
/// and can be re-read; through a speaker in a room it is simply gone. An agent
/// that cannot tell the difference has to write for the worse case always, or
/// gets it wrong half the time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum VoiceSurface {
    /// A phone, laptop, or terminal: spoken *and* shown.
    Screen,
    /// A satellite speaker in a room. Nothing is written down.
    Speaker,
}

impl VoiceSurface {
    /// Which surface a channel is.
    ///
    /// The gateway's own conversational channels have a screen; anything else
    /// reaching voice mode is a satellite, which is the safer default — an
    /// agent that wrongly assumes a screen strands the listener with detail
    /// they cannot recover, while one that wrongly assumes a speaker is merely
    /// a little plainer than it needed to be.
    pub(crate) fn of(channel: &str) -> Self {
        if workforce_common::channels::is_conversational(channel) {
            Self::Screen
        } else {
            Self::Speaker
        }
    }

    fn rules(self) -> &'static str {
        match self {
            Self::Screen => {
                "\
- Their phone or computer, so your reply is on screen as well as spoken. A name,
  number or spelling they miss can be re-read, so say those as you would to
  anyone; what is only readable (code, a command, a link, a path) is still never
  read out — see above."
            }
            Self::Speaker => {
                "\
- A speaker in a room, with no screen. Nothing you say is written down anywhere.
- They cannot re-read or scroll back, so anything heard once has to be
  understood once. Do not spell out URLs, long codes, or file paths.
- Never refer to something visual — no \"as shown above\", no \"the list below\".
- They are probably doing something else in the room. Lead with the answer, and
  keep it to a couple of sentences unless they asked for more.
- If the answer really needs detail they would want to keep, say so and offer to
  send it to their phone rather than reciting it.
- The microphone stays open after you speak, so the person can follow up without
  saying the wake word again. That is useful mid-conversation and a nuisance
  once the exchange is over: it leaves the device listening to an empty room.
  End your reply with <end_conversation> when there is nothing left to say.
  The marker is removed before your words are spoken, so it is never heard.
- Use it whenever you have simply done what was asked and no answer is
  outstanding — playing music, stopping it, setting something, confirming
  something. \"Playing Man in the Mirror\" is finished business: end it.
- Use it when they sign off — \"catch you later\", \"thanks, that's all\",
  \"goodnight\". Say goodbye back and end it in the same reply; do not wait to
  be told twice.
- Do not use it when you have asked a question, offered a choice, or said
  anything the person would naturally answer. When in doubt, leave it open: a
  conversation that stays open a little too long costs a moment of silence,
  where one closed mid-thought costs them starting again."
            }
        }
    }
}

/// Build the voice-mode system prompt directive.
///
/// Injected into the dynamic portion of the system prompt when the turn was
/// spoken and its answer will be heard. Contains general conversational
/// rules, where the person is listening, and what the voice reading it
/// (`provider` speaking `model`, when the client said) can do — for a voice
/// that takes directions, exactly the ones it performs, from the table its
/// text is checked against before it is spoken.
pub(crate) fn voice_mode_directive(provider: &str, model: Option<&str>, channel: &str) -> String {
    let markup = workforce_agent::llm::speech::Markup::of(provider, model);
    let provider_rules = markup.guidance().unwrap_or_else(|| {
        let rules = match workforce_agent::llm::speech::canonical_speech_provider(provider) {
            Some("elevenlabs") => {
                "\
TTS provider: ElevenLabs
- Keep text clean and natural — the voice model handles emotion automatically."
            }
            Some("soniox") => {
                "\
TTS provider: Soniox
- Keep text clean and natural. Numbers, email addresses, and codes are
  pronounced accurately as written."
            }
            _ if provider == "kokoro" => {
                "\
TTS provider: Kokoro
- Plain text, simple sentence structure."
            }
            _ => {
                "\
TTS provider: System TTS
- Plain text only. Use simple sentence structure for best clarity."
            }
        };
        format!(
            "{rules}\n- No voice directions in parentheses or brackets: this voice performs \
             none, so they would be removed before you are heard."
        )
    });

    format!(
        "[VOICE MODE ACTIVE — User is listening via TTS. Adapt your responses:]\n\n\
         General voice formatting:\n\
         - Talk the way you would to someone across the table: warm, natural and \
           conversational, in short spoken sentences. Contractions are fine.\n\
         - Never read out code, symbols, punctuation marks, markdown, URLs or file \
           paths. Say what they are in words instead — \"the settings file in your \
           home folder\", \"the link to the pull request\" — and summarise code by \
           what it does, never line by line.\n\
         - When the detail itself matters (a command to run, the code, a list to \
           keep), give the gist aloud and offer to put the details in the chat.\n\
         - No emojis, markdown, bullet lists, headers, tables or code blocks\n\
         - Use proper punctuation for natural pauses\n\
         - Avoid abbreviations and acronyms that read badly aloud\n\
         - Keep responses concise and flowing\n\
         - If you need to convey a list, say it naturally: \
           \"There are three things — first X, then Y, and finally Z\"\n\
         - Say exact values exactly: times, dates, numbers, prices, names and \
           codes are repeated as they are given, never rounded, approximated or \
           recalled from memory. Speaking casually applies to how you say a \
           thing, never to what it is — \"about half eight\" for 8:22 is a \
           wrong answer, not a friendly one, and a listener cannot \
           check it.\n\
         - Asked the time or the date, check it with a tool before answering, \
           exactly as you would anywhere else. Being quick is not worth being \
           wrong, and this is the one question where answering from what you \
           already have in front of you has repeatedly produced a time from \
           hours earlier in the same conversation.\n\n\
         Where they are listening:\n\
         {surface_rules}\n\n\
         {provider_rules}",
        surface_rules = VoiceSurface::of(channel).rules()
    )
}

/// The note on a dictated turn: the person spoke it and reads the answer.
pub(crate) const DICTATED_TURN_NOTE: &str = "[DICTATED — the person spoke this message and \
     it was transcribed, so a word may be misheard: read past slips, and ask when something \
     that matters is ambiguous. They will read your answer, not hear it: reply in writing as \
     you normally would.]";

pub(crate) fn relative_date(
    ts: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let days = (now - ts).num_days();
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=6 => format!("{days} days ago"),
        7..=13 => "last week".to_string(),
        _ => ts.format("%b %d").to_string(),
    }
}

/// Render the per-turn context block listing `/secret`-set credential
/// names. `None` when empty. Names only — never the values.
pub(crate) fn format_named_secret_block(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let list = names
        .iter()
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "[NAMED SECRETS — the user set these via `cmd: secret <name> <value>` (the thread-safe \
         form; if you ever need a new one, instruct that, never the `/secret` slash command). \
         Pass one as the placeholder \
         `[[secret:<name>]]` in a credential field of a secret-sink tool — `vault.set`, and \
         `http.request`'s `headers`, `query`, `body`, or `body_text`, where it resolves at any \
         depth of a JSON body so a payload can mix credentials with ordinary values; the \
         system swaps in the real value at the tool boundary. NEVER reveal, repeat, type, or \
         echo a secret's value in a reply — not even to confirm it or ask the user to re-enter \
         it. These stay available in this channel, so never tell the user one has expired or \
         ask them to paste it again.]\n{list}"
    ))
}

/// Per-turn awareness block for the named secrets reachable from
/// `session_key`, so the agent doesn't ask the user to repaste them.
pub(crate) fn named_secret_context(session_key: &str) -> Option<String> {
    let names = workforce_common::secret_marker::global_cache()?.list_named(session_key);
    format_named_secret_block(&names)
}

/// Opens "Your Connected Skills". It says whose connections these are and
/// that they outrank the shared catalogue's badge: the model reads the
/// catalogue's [not configured] first, far above.
pub(crate) const CONNECTED_SKILLS_HEADER: &str = "\n\n## Your Connected Skills\n\n\
     Connected by the person you are talking to, with their own credentials. Each is ready \
     for them, whatever badge \"Loadable Skills\" above gives it.\n\n";

/// Opens the same block for a turn nobody is talking in: an automated run
/// whose tools act with a person's credentials.
pub(crate) const RUN_SKILLS_HEADER: &str = "\n\n## Skills Ready For This Run\n\n\
     This run's tools act with the credentials of the person it runs for. Each skill below \
     is ready for it, whatever badge \"Loadable Skills\" above gives it.\n\n";

/// The skills ready for this user that the shared catalogue cannot badge
/// [ready]: OAuth skills, and skills whose credentials only this user holds.
/// `None` when there is none. In the person's `own_conversation` the block
/// names their accounts; anywhere else it says only that the skill is ready.
pub(crate) fn render_connected_skills(
    for_user: &[crate::skill_loader::SkillCredentialStatus],
    catalog: &[crate::skill_loader::BuiltinSkillMeta],
    shared_ready: &std::collections::HashMap<String, bool>,
    own_conversation: bool,
) -> Option<String> {
    let is_oauth = |name: &str| {
        catalog.iter().any(|meta| {
            meta.name == name && meta.managed_by == crate::skill_loader::CredManaged::Oauth
        })
    };
    let lines: String = for_user
        .iter()
        .filter(|status| status.ready)
        .filter(|status| {
            is_oauth(&status.name) || !shared_ready.get(&status.name).copied().unwrap_or(false)
        })
        .map(|status| {
            let name = &status.name;
            if !own_conversation {
                return format!(
                    "- **{name}**: READY. Call `system.enable_skill('{name}')` to use it.\n"
                );
            }
            let accounts = match status.configured_workspaces.is_empty() {
                true => String::new(),
                false => format!(" (accounts: {})", status.configured_workspaces.join(", ")),
            };
            format!(
                "- **{name}**: connected for you{accounts} — READY. Call \
                 `system.enable_skill('{name}')` to use it; do not ask the user to set it up.\n"
            )
        })
        .collect();
    let header = if own_conversation {
        CONNECTED_SKILLS_HEADER
    } else {
        RUN_SKILLS_HEADER
    };
    (!lines.is_empty()).then(|| format!("{header}{lines}"))
}
