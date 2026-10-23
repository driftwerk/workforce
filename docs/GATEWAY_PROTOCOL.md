# Workforce Gateway Protocol

WebSocket + REST protocol for client connections (Clawdy iOS, web UI, etc.).

## Connection

### WebSocket
```
ws://<host>:8091/ws?token=<auth_token>
```

### REST (fallback)
```
POST http://<host>:8091/api/chat
Authorization: Bearer <auth_token>
Content-Type: application/json

{"message": "Hello!", "channel": "clawdy"}
```

> **Production:** put the hub behind TLS and use `wss://<host>/ws` and `https://<host>/api/chat`. Plain `ws://` / `http://` only apply to local-loopback and private-network development.

## Authentication

All connections require a bearer token:
- **WebSocket**: Pass as `?token=<token>` query parameter
- **REST**: Pass as `Authorization: Bearer <token>` header

Token is configured via:
- `workforce.toml`: `[gateway] auth_token = "..."`
- Environment variable: `WORKFORCE_GATEWAY_TOKEN`
- Auto-generated on startup if neither is set

## WebSocket Message Format

### Client → Server

```json
{
  "type": "chat",
  "content": "Hello, how are you?",
  "session_id": "optional-session-id",
  "channel": "clawdy"
}
```

**Message types:**
| Type | Description |
|------|-------------|
| `chat` | Send a message for the agent to respond to |
| `cancel` | Stop the session's live turn (`session_id`) |
| `permission_response` | Answer a `permission_prompt` (`request_id`, `choice`) |
| `ping` | Keepalive ping (server responds with `pong`) |

A `chat` sent while the session's turn is still running is absorbed into it
(the server answers `injected`); once that turn's answer is out, it starts the
next turn instead. A `chat` with attachments during a live turn is refused with
an `error`. Set `"answers_approvals": true` on a `chat` when the person can
answer a `permission_prompt` for it; otherwise tool calls needing approval are
refused at once.

`cancel` is fully handled before the next frame is read: the turn is stopped,
settled and released, so a `chat` sent right after it starts a fresh turn.
Nothing of the cancelled turn follows the `cancel` — no `stream_end` — so the
client finalizes it locally.

### Server → Client

```json
{
  "type": "message",
  "content": "I'm doing well! How can I help?",
  "session_id": "abc-123",
  "error": null
}
```

**Message types:**
| Type | Description |
|------|-------------|
| `connected` | Initial connection acknowledgement |
| `message` | Agent response |
| `typing` | Agent is processing (show typing indicator) |
| `stream_start` | Beginning of streamed response |
| `stream_delta` | Chunk of streamed response |
| `stream_end` | End of streamed response |
| `injected` | A `chat` was absorbed into the running turn |
| `permission_prompt` | A tool call awaits the person's approval |
| `pong` | Response to ping |
| `error` | Error occurred |

## Streaming (future)

When streaming is enabled, responses arrive as chunks:

```json
{"type": "stream_start", "session_id": "abc-123"}
{"type": "stream_delta", "content": "I'm ", "session_id": "abc-123"}
{"type": "stream_delta", "content": "doing ", "session_id": "abc-123"}
{"type": "stream_delta", "content": "well!", "session_id": "abc-123"}
{"type": "stream_end", "session_id": "abc-123"}
```

## REST Endpoints

### POST /api/chat
Send a message and get a response (non-streaming).

**Request:**
```json
{
  "message": "What's the weather like?",
  "channel": "clawdy",
  "session_id": "optional"
}
```

**Response:**
```json
{
  "response": "I'd need to check a weather service...",
  "session_id": "abc-123"
}
```

### GET /api/sessions
List active sessions.

**Response:**
```json
{
  "sessions": [
    {
      "id": "abc-123",
      "label": "clawdy:user-1",
      "channel": "clawdy",
      "owner": "user-1",
      "messages": 42,
      "total_tokens": 15000,
      "updated_at": "2026-02-10T14:30:00Z"
    }
  ]
}
```

## Reconnection

1. Client connects to `/ws?token=<token>`
2. Server sends `{"type": "connected"}`
3. If connection drops, client reconnects with same token
4. Session state is preserved server-side
5. Client can resume by sending messages to same session

## Error Handling

Errors are sent as:
```json
{
  "type": "error",
  "content": "Error description",
  "error": "detailed_error_code"
}
```

Common errors:
- `401 Unauthorized` — Invalid or missing token
- `429 Too Many Requests` — Rate limit exceeded
- `500 Internal Server Error` — Agent processing failed
