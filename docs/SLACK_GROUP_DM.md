# Slack Multi-Person DMs (Group DMs / `mpim`)

How to make an agent respond in a Slack **group DM** — a multi-person direct
message (Slack type `mpim`), as opposed to a 1:1 DM (`im`) or a channel.

## TL;DR

**No workforce code change is needed.** The event handler already routes
`message` / `app_mention` events for any channel type, and an explicit
`@mention` always passes the activation gate. If the agent doesn't respond in a
group DM, the event is **not being delivered by Slack** — which is an app-config
problem, not a code problem.

The usual cause: the `mpim:*` **scopes** were added, but the **`message.mpim`
event subscription** was not (and/or the app wasn't reinstalled afterwards).
Adding a scope does not subscribe you to the event.

## Setup checklist

In your Slack App at [api.slack.com/apps](https://api.slack.com/apps):

1. **OAuth & Permissions → Bot Token Scopes** — add:
   - `app_mentions:read` — so an `@mention` fires an `app_mention` event
   - `mpim:history` — so group-DM message text is delivered
   - `mpim:read` — list/resolve group-DM conversations
   - (`mpim:write` only if the agent needs to *open* group DMs itself)

2. **Event Subscriptions → Subscribe to bot events** — add:
   - **`message.mpim`** ← this is the one that actually delivers group-DM messages
   - `app_mention` (if not already there)

3. **Reinstall the app to the workspace.** Scope and subscription changes do
   **not** take effect until you reinstall — the existing bot token keeps the
   old grant.

4. **Make sure the bot is a member of the group DM.** Unlike channels, you
   **cannot** `/invite @bot` into an existing group DM. The app must be added
   when the group DM is created. If it isn't a member, start a new group DM that
   includes it.

## Why it works without code changes

The relevant path in `crates/skills/src/adapters/slack/event_handler.rs`:

- **Dispatch** (`handle` → match on `event_type`): `message` and `app_mention`
  are both routed to `handle_message`, with **no channel-type filter**.
- **Activation gate** (`handle_message`):
  ```rust
  let is_dm = event.channel_type.as_deref() == Some("im");   // mpim is NOT a DM
  let is_mentioned =
      self.message_mentions_bot(&text) || event.event_type == "app_mention";
  ```
  `should_respond_with` returns `true` as soon as `is_mentioned` is set — it
  never looks at `is_dm`. So even though a group DM is classified as *not* a DM
  (`channel_type == "mpim"`, not `"im"`), an explicit `@mention` still triggers a
  response.

In other words: a group DM behaves like a channel for activation purposes —
the agent responds when mentioned, not to every message — and that's the
intended behaviour.

## Gotchas

- **Scope ≠ subscription.** The single most common mistake. You need both the
  `mpim:*` scope *and* the `message.mpim` event subscription, then a reinstall.
- **Channel allowlist drops group DMs.** If a workspace config sets a non-empty
  `channels = [...]` allowlist, the group-DM channel ID (`G…`/`C…`) is dropped at
  the allowlist check in `handle_message` unless it's listed. Either leave the
  allowlist empty or add the group-DM's channel ID.
- **Unmentioned messages won't get a reply.** Because a group DM is not treated
  as a 1:1 DM, the agent only responds when `@mentioned` (or an activation
  keyword matches), the same as in a channel. This is intentional.
- **Confirm delivery from the logs.** After reinstalling, if it still doesn't
  respond, check the hub logs for the incoming event. If no event arrives for
  the group DM, the problem is still Slack-side (subscription / membership), not
  workforce.

## See also

- [SLACK.md](SLACK.md) — full Slack integration reference (scopes, response
  modes, Socket Mode details).
- The canonical recommended app manifest lives in
  `crates/hub/src/slack_tool.rs` (`slack.app_manifest` tool) and already
  includes the `mpim:*` scopes and the `message.mpim` subscription.
