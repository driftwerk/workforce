---
name: figma
version: 1.3.0
description: Figma design file API client — authentication, rate limiting, structured requests, and design brief generation
author: driftwerk
requires:
  bins: [curl, jq]
  vault:
    - path: skills/figma/token
      description: Figma Personal Access Token
      required: true
tools:
  - exec.run
  - vault.get
  - figma.file_map
  - figma.inspect
  - figma.export
  - figma.brief
---

# Figma API Client

Core HTTP client layer for the Figma REST API. Handles authentication, rate limiting, error mapping, and request logging. All operations are **read-only** — no write or mutation endpoints.

## Constraints

- **Read-only.** This skill only performs GET requests against the Figma API. No POST, PUT, PATCH, or DELETE.
- **Token security.** Never log, echo, or include the Figma API token in output. Always redact before displaying request details.
- **Rate limiting.** Figma enforces ~30 requests per minute. Respect this limit with spacing and backoff.
- **Fail loudly.** Every request must check the HTTP status code and return a clear, actionable error message on failure.

## Authentication

### Step 1 — Retrieve the token

Fetch the Figma Personal Access Token from the vault:

```
vault.get: path=skills/figma/token
```

If the vault returns an error or empty value, **STOP** and report:

> Figma token not configured. Store your Figma Personal Access Token with:
> `vault.set path=skills/figma/token value=<your-token>`

### Step 2 — Validate the token

Test the token with a call to the `/v1/me` endpoint:

```
exec.run: curl -s -w "\n%{http_code}" -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/me"
```

Parse the last line as the HTTP status code and everything before it as the JSON response body.

- **200** — Token is valid. Extract and log the user's `handle` and `email` for confirmation.
- **403** — Token is invalid or expired. Report: "Figma token is invalid or expired. Generate a new Personal Access Token at https://www.figma.com/developers/api#access-tokens and update it with `vault.set path=skills/figma/token value=<new-token>`."
- **Any other status** — Report the status code and response body.

## Base Request Pattern

All Figma API requests follow this template:

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/{endpoint}"
```

### Parsing the response

The `-w "\n%{http_code}"` flag appends the HTTP status code on a new line after the response body. To parse:

1. Split the output on the last newline.
2. The last line is the HTTP status code (e.g. `200`, `404`, `429`).
3. Everything before the last line is the JSON response body.

### Timeouts

All requests use `--max-time 30` (30-second timeout). If a request times out, report:

> Request timed out after 30 seconds. The Figma API may be experiencing issues. Retry after a short wait.

### Extracting data with jq

Pipe the JSON response body through `jq` for structured extraction. Examples:

```bash
# Get file name
echo "<RESPONSE_BODY>" | jq -r '.name'

# Get top-level node IDs
echo "<RESPONSE_BODY>" | jq -r '.document.children[] | .id'

# Get image URLs from an export response
echo "<RESPONSE_BODY>" | jq -r '.images | to_entries[] | "\(.key): \(.value)"'
```

Always validate that `jq` output is not `null` before using it. If a field is missing, report which field was expected and what was actually returned.

## Rate Limiting

Figma enforces a rate limit of approximately **30 requests per minute**.

### Burst protection

Between consecutive requests, enforce a **minimum 2-second pause**:

```
exec.run: sleep 2
```

This prevents bursting through the rate limit on rapid sequential operations.

### Backoff on 429

When a request returns HTTP **429** (Too Many Requests):

1. Log: "Rate limited by Figma API. Backing off..."
2. Wait **60 seconds**: `exec.run: sleep 60`
3. Retry the same request.
4. If 429 again, wait **120 seconds** and retry once more.
5. If 429 a third time, **STOP** and report:

> Rate limit exceeded after 3 retries. Wait a few minutes before making more Figma API requests.

Maximum retries on 429: **3 attempts** (initial + 2 retries with escalating backoff: 60s, 120s).

## Error Handling

Map every non-200 HTTP status code to an actionable message:

| Status | Meaning | Message |
|--------|---------|---------|
| 400 | Bad Request | "Bad request — check endpoint parameters. Verify the file key and any node IDs are correct." |
| 403 | Forbidden | "Token invalid, expired, or lacks access to this file. Check your Figma PAT permissions and that the file is shared with your account." |
| 404 | Not Found | "File or node not found — verify the file key exists and you have access. File keys are the alphanumeric string in the Figma URL after `/file/`." |
| 429 | Rate Limited | Follow the backoff procedure in the Rate Limiting section above. |
| 500 | Internal Server Error | "Figma API server error (500). This is a Figma-side issue. Retry after 30 seconds." |
| 502 | Bad Gateway | "Figma API returned 502 Bad Gateway. Retry after 30 seconds." |
| 503 | Service Unavailable | "Figma API is temporarily unavailable (503). Retry after 60 seconds." |
| Other | Unknown | "Unexpected HTTP status {code} from Figma API. Response: {body}" |

For any 5xx error, retry **once** after a 30-second wait. If the retry also returns 5xx, report the error and **STOP**.

## Logging

### Request logging

Before each request, log (to the conversation, not to a file):

```
→ GET /v1/{endpoint} ...
```

Do **not** include the token value in any logged output.

### Response logging

After each request, log:

```
← {status_code} ({response_size} bytes)
```

Where `response_size` is the character length of the response body.

### Error logging

On any error, log the full error context:

```
✗ GET /v1/{endpoint} → {status_code}: {error_message}
```

## Endpoint Reference

This skill supports the following Figma REST API endpoints. All are **read-only GET requests**.

### GET /v1/files/:key

Retrieve metadata and the full document tree for a Figma file.

**Parameters:**
- `:key` (path, required) — The file key from the Figma URL (e.g. `abc123DEF456` from `figma.com/file/abc123DEF456/...`)

**Optional query parameters:**
- `depth` — How deep to traverse the document tree (integer). Use to limit response size for large files.
- `geometry=paths` — Include vector path data.

**Example:**

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/files/abc123DEF456?depth=2"
```

**Response fields of interest:**
- `.name` — File name
- `.lastModified` — Last modification timestamp
- `.document` — Root document node
- `.document.children` — Pages in the file

### GET /v1/files/:key/nodes

Retrieve specific nodes from a Figma file by their IDs.

**Parameters:**
- `:key` (path, required) — The file key
- `ids` (query, required) — Comma-separated list of node IDs (e.g. `1:2,3:4`)

**Optional query parameters:**
- `depth` — Traversal depth from the specified nodes.

**Example:**

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/files/abc123DEF456/nodes?ids=1:2,3:4"
```

**Response fields of interest:**
- `.nodes["1:2"]` — The requested node and its children
- `.nodes["1:2"].document` — The node's document subtree
- `.nodes["1:2"].components` — Component metadata if the node contains components

### GET /v1/images/:key

Export renderings of nodes from a Figma file as images.

**Parameters:**
- `:key` (path, required) — The file key
- `ids` (query, required) — Comma-separated list of node IDs to export
- `format` (query, optional) — Export format: `jpg`, `png`, `svg`, or `pdf` (default: `png`)
- `scale` (query, optional) — Image scale factor: `0.01` to `4` (default: `1`)

**Example:**

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/images/abc123DEF456?ids=1:2,3:4&format=png&scale=2"
```

**Response fields of interest:**
- `.images` — Object mapping node IDs to temporary image URLs
- `.err` — Error string if any node failed to render (null on success)

**Note:** The returned image URLs are temporary and expire. Download or use them promptly.

## Procedure: Making a Request

When an agent needs to call the Figma API, follow this sequence:

1. **Retrieve the token** — `vault.get path=skills/figma/token`. If unavailable, stop and instruct the user to configure it.
2. **Check rate limit** — If this is not the first request in the session, wait at least 2 seconds since the last request.
3. **Build the request** — Construct the curl command using the base request pattern with the appropriate endpoint and parameters.
4. **Execute** — Run via `exec.run`. Parse the status code and response body.
5. **Check status** — If non-200, apply error handling (including 429 backoff). If 5xx, retry once.
6. **Log** — Log the request and response per the logging section.
7. **Parse** — Extract the needed data from the JSON response using `jq`.
8. **Return** — Present the parsed data to the user in a clear format.

## Tool: figma.file_map

Returns a structured "table of contents" for a Figma file — pages, sections, top-level frames, and component sets with their dimensions, node IDs, child counts, and direct Figma URLs. Output is optimised for agent context windows: deep nesting is collapsed and large frame lists are summarised.

### Input

- `file_key` (required) — Figma file key OR full Figma URL. If a URL is provided, the file key is extracted automatically.
- `page_filter` (optional) — Page name substring to filter results to matching pages only (case-insensitive).

### Step 1 — Parse the file key

If the input contains `figma.com`, extract the file key from the URL path:

```bash
FILE_KEY=$(echo "<input>" | sed -n 's|.*figma\.com/\(design\|file\|board\)/\([a-zA-Z0-9]*\).*|\2|p')
```

If the input does not contain `figma.com`, treat the entire input as the file key:

```bash
FILE_KEY="<input>"
```

**Validate** the file key:
- Must be non-empty
- Must contain only alphanumeric characters (a-z, A-Z, 0-9)

If validation fails, **STOP** and report:

> Could not extract Figma file key. Expected a URL like `figma.com/design/ABC123/...` or a raw file key like `ABC123`.

### Step 2 — Retrieve the token

```
vault.get: path=skills/figma/token
```

If the token is unavailable, follow the error handling in the Authentication section.

### Step 3 — Fetch the file structure

Enforce rate limiting — if this is not the first Figma API call in the session, wait at least 2 seconds:

```
exec.run: sleep 2
```

Fetch the file with `depth=2` to get pages and their direct children only:

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/files/<FILE_KEY>?depth=2"
```

Log the request per the Logging section:

```
→ GET /v1/files/<FILE_KEY>?depth=2 ...
```

Parse the response: split on the last newline to separate the JSON body from the HTTP status code.

- **200** — Continue to Step 4.
- **404** — "File not found — verify the file key `<FILE_KEY>` exists and your token has access. File keys are the alphanumeric string in the Figma URL after `/design/` or `/file/`."
- **Any other non-200 status** — Apply the standard error handling from the Error Handling section.

Log the response:

```
← <status_code> (<response_size> bytes)
```

### Step 4 — Transform with jq

Pipe the response body through `jq` to extract the structured tree. Use the `FILE_KEY` to construct Figma URLs for each node.

**Without page filter:**

```bash
echo '<RESPONSE_BODY>' | jq --arg fk "<FILE_KEY>" '
{
  file_name: .name,
  last_modified: .lastModified,
  pages: [.document.children[] | {
    name: .name,
    id: .id,
    children_count: (.children // [] | length),
    items: [(.children // [])[] |
      select(.type == "FRAME" or .type == "SECTION" or .type == "COMPONENT_SET" or .type == "COMPONENT") |
      {
        name: .name,
        id: .id,
        type: .type,
        width: (.absoluteBoundingBox.width // null),
        height: (.absoluteBoundingBox.height // null),
        children_count: (.children // [] | length),
        url: ("https://www.figma.com/design/" + $fk + "?node-id=" + (.id | gsub(":"; "-")))
      }
    ]
  }]
}
'
```

**With page filter** — add a `select` to filter pages by name (case-insensitive substring match):

```bash
echo '<RESPONSE_BODY>' | jq --arg fk "<FILE_KEY>" --arg pf "<PAGE_FILTER>" '
{
  file_name: .name,
  last_modified: .lastModified,
  pages: [.document.children[] |
    select(.name | ascii_downcase | contains($pf | ascii_downcase)) |
    {
      name: .name,
      id: .id,
      children_count: (.children // [] | length),
      items: [(.children // [])[] |
        select(.type == "FRAME" or .type == "SECTION" or .type == "COMPONENT_SET" or .type == "COMPONENT") |
        {
          name: .name,
          id: .id,
          type: .type,
          width: (.absoluteBoundingBox.width // null),
          height: (.absoluteBoundingBox.height // null),
          children_count: (.children // [] | length),
          url: ("https://www.figma.com/design/" + $fk + "?node-id=" + (.id | gsub(":"; "-")))
        }
      ]
    }
  ]
}
'
```

If a page filter was provided and the result contains zero pages, list all available page names from the response and report:

> No pages matching '<filter>' found. Available pages: <comma-separated list of page names>.

### Step 5 — Truncate large pages

If any page has more than 25 items in its `items` array, truncate to the first 20 and note the remainder:

- Keep the first 20 items.
- Append a note: `… and N more frames/sections` where N is the count of remaining items.

### Step 6 — Format output

Present the result in this format:

```
📄 <file_name> (last modified: <formatted_date>)

📑 Page: <page_name> (<children_count> top-level nodes)
  ├─ 🖼 <frame_name> (<width>×<height>) — <children_count> layers [<url>]
  ├─ 📂 <section_name> — <children_count> children [<url>]
  ├─ 🧩 <component_set_name> (<width>×<height>) — <children_count> variants [<url>]
  └─ 🖼 <frame_name> (<width>×<height>) — <children_count> layers [<url>]

📑 Page: <page_name_2> (<children_count> top-level nodes)
  ├─ ...
  └─ … and N more frames/sections
```

**Icon mapping:**
- `FRAME` → 🖼 — show dimensions, label children as "layers"
- `SECTION` → 📂 — no dimensions, label children as "children"
- `COMPONENT_SET` → 🧩 — show dimensions, label children as "variants"
- `COMPONENT` → 🧩 — show dimensions, label children as "layers"

**Tree connectors:**
- Use `├─` for all items except the last one in each page.
- Use `└─` for the last item (or the truncation note).

**Date formatting:** Convert the `lastModified` ISO timestamp to a human-readable format (e.g. "27 Feb 2026, 07:23").

### Step 7 — Handle errors

This tool uses the standard error handling from the Error Handling section for all API errors. Additional tool-specific errors:

| Condition | Message |
|-----------|---------|
| File key extraction fails | "Could not extract Figma file key. Expected a URL like `figma.com/design/ABC123/...` or a raw file key like `ABC123`." |
| No pages match filter | "No pages matching '<filter>' found. Available pages: <list>." |
| File has no pages | "File `<file_name>` contains no pages." |
| Response missing `.document` | "Unexpected response structure — `.document` field is missing. The file key may be invalid." |

## Tool: figma.inspect

Fetches a single node's full subtree from a Figma file and extracts structured design properties — dimensions, fills, strokes, border radius, auto-layout, spacing, typography, effects, and children. Output is formatted as a readable design specification that agents can use directly to write implementation code.

### Input

- `file_key` (required) — Figma file key OR full Figma URL. If a URL is provided, the file key is extracted automatically.
- `node_id` (required) — Figma node ID to inspect (e.g. `1:2`). Use `figma.file_map` to discover node IDs.

### Step 1 — Parse the file key

If the input contains `figma.com`, extract the file key from the URL path:

```bash
FILE_KEY=$(echo "<input>" | sed -n 's|.*figma\.com/\(design\|file\|board\)/\([a-zA-Z0-9]*\).*|\2|p')
```

If the input does not contain `figma.com`, treat the entire input as the file key:

```bash
FILE_KEY="<input>"
```

**Validate** the file key:
- Must be non-empty
- Must contain only alphanumeric characters (a-z, A-Z, 0-9)

If validation fails, **STOP** and report:

> Could not extract Figma file key. Expected a URL like `figma.com/design/ABC123/...` or a raw file key like `ABC123`.

### Step 2 — Retrieve the token

```
vault.get: path=skills/figma/token
```

If the token is unavailable, follow the error handling in the Authentication section.

### Step 3 — Fetch node data

Enforce rate limiting — if this is not the first Figma API call in the session, wait at least 2 seconds:

```
exec.run: sleep 2
```

Fetch the node with its full subtree:

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/files/<FILE_KEY>/nodes?ids=<NODE_ID>"
```

**Note:** The node ID must be URL-encoded if it contains `:` — replace `:` with `%3A` in the URL (e.g. `1:2` becomes `1%3A2`).

Log the request per the Logging section:

```
→ GET /v1/files/<FILE_KEY>/nodes?ids=<NODE_ID> ...
```

Parse the response: split on the last newline to separate the JSON body from the HTTP status code.

- **200** — Continue to Step 4.
- **404** — "Node or file not found — verify the file key `<FILE_KEY>` and node ID `<NODE_ID>` are correct. Use `figma.file_map` to discover valid node IDs."
- **Any other non-200 status** — Apply the standard error handling from the Error Handling section.

Log the response:

```
← <status_code> (<response_size> bytes)
```

### Step 4 — Extract the node document

Parse the response to extract the node document. The response structure is `.nodes["<NODE_ID>"].document`:

```bash
NODE_DOC=$(echo '<RESPONSE_BODY>' | jq --arg nid "<NODE_ID>" '.nodes[$nid].document')
```

**Validate** the extraction:
- If `NODE_DOC` is `null` or empty, the node ID was not found. Report:

> Node `<NODE_ID>` not found in file `<FILE_KEY>`. Use `figma.file_map` to discover valid node IDs for this file.

### Step 5 — Transform with jq

Pipe the node document through `jq` to extract structured design properties. Use the `FILE_KEY` and `NODE_ID` to construct a direct Figma URL.

```bash
echo '<NODE_DOC>' | jq --arg fk "<FILE_KEY>" '
def to_hex:
  def h: if . < 10 then . + 48 else . + 87 end;
  [(. / 16 | floor | h), (. % 16 | h)] | implode;

def rgb_to_hex:
  "#" + ((.r * 255 | floor) | to_hex) +
        ((.g * 255 | floor) | to_hex) +
        ((.b * 255 | floor) | to_hex);

def fmt_color:
  if .type == "SOLID" and .color then {
    hex: (.color | rgb_to_hex),
    opacity: (((.color.a // 1) * 100 | round | tostring) + "%")
  } else null end;

{
  name: .name,
  type: .type,
  id: .id,
  url: ("https://www.figma.com/design/" + $fk + "?node-id=" + (.id | gsub(":"; "-"))),
  component_id: (.componentId // null),

  dimensions: (if .absoluteBoundingBox then {
    width: .absoluteBoundingBox.width,
    height: .absoluteBoundingBox.height
  } else null end),

  background: [(.fills // [])[] | select(.visible != false) | {
    type: .type,
    color: (if .type == "SOLID" and .color then (.color | rgb_to_hex) else null end),
    opacity: (if .type == "SOLID" and .color then
      ((.color.a // 1) * 100 | round | tostring) + "%"
    else null end),
    opacity_float: (.opacity // 1),
    blend_mode: (.blendMode // null),
    image_ref: (.imageRef // null),
    scale_mode: (.scaleMode // null)
  }],

  strokes: [(.strokes // [])[] | select(.visible != false) | {
    type: .type,
    color: (if .type == "SOLID" and .color then (.color | rgb_to_hex) else null end),
    opacity: (if .type == "SOLID" and .color then
      ((.color.a // 1) * 100 | round | tostring) + "%"
    else null end)
  }],
  stroke_weight: (.strokeWeight // null),
  stroke_align: (.strokeAlign // null),

  border_radius: (
    if .rectangleCornerRadii then {
      top_left: .rectangleCornerRadii[0],
      top_right: .rectangleCornerRadii[1],
      bottom_right: .rectangleCornerRadii[2],
      bottom_left: .rectangleCornerRadii[3]
    } elif .cornerRadius then {
      uniform: .cornerRadius
    } else null end
  ),

  layout: (if .layoutMode then {
    mode: .layoutMode,
    primary_axis_align: (.primaryAxisAlignItems // null),
    counter_axis_align: (.counterAxisAlignItems // null),
    layout_align: (.layoutAlign // null),
    layout_grow: (.layoutGrow // null),
    layout_wrap: (.layoutWrap // null)
  } else null end),

  spacing: {
    padding_top: (.paddingTop // null),
    padding_right: (.paddingRight // null),
    padding_bottom: (.paddingBottom // null),
    padding_left: (.paddingLeft // null),
    item_spacing: (.itemSpacing // null)
  },

  effects: [(.effects // [])[] | select(.visible != false) | {
    type: .type,
    radius: (.radius // null),
    offset: (if .offset then { x: .offset.x, y: .offset.y } else null end),
    color: (if .color then (.color | rgb_to_hex) else null end),
    opacity: (if .color then
      ((.color.a // 1) * 100 | round | tostring) + "%"
    else null end),
    spread: (.spread // null)
  }],

  typography: (if .type == "TEXT" then {
    font_family: (.style.fontFamily // null),
    font_weight: (.style.fontWeight // null),
    font_size: (.style.fontSize // null),
    line_height_px: (.style.lineHeightPx // null),
    letter_spacing: (.style.letterSpacing // null),
    text_align_horizontal: (.style.textAlignHorizontal // null),
    text_align_vertical: (.style.textAlignVertical // null),
    text_decoration: (.style.textDecoration // null),
    text_case: (.style.textCase // null),
    characters: (.characters // null)
  } else null end),

  children: [(.children // [])[:50] | .[] | {
    name: .name,
    type: .type,
    id: .id,
    dimensions: (if .absoluteBoundingBox then {
      width: .absoluteBoundingBox.width,
      height: .absoluteBoundingBox.height
    } else null end),
    component_id: (.componentId // null)
  }],
  children_count: ((.children // []) | length)
}
'
```

**Colour conversion:** The `to_hex` helper converts Figma's 0-1 float RGB values to hexadecimal. Each component is multiplied by 255, floored, then converted to a two-character hex string. Opacity is derived from `color.a` and expressed as a percentage (e.g. `100%`, `50%`). The output format is `#RRGGBB @ NN%`.

If the jq transform fails or returns null for the entire output, fall back to presenting the raw node document and report:

> ⚠️ jq transform failed for this node. Showing raw node data. The node type `<type>` may have an unexpected structure.

### Step 6 — Format output

Present the extracted data as a structured, human-readable design specification. Only include sections that have non-null data.

```
🔍 <name> (<type>) — <width>×<height>
   Figma: <url>

📐 Dimensions
   Width: <width>px
   Height: <height>px

🎨 Background
   ├─ <fill_type>: <color> @ <opacity>
   └─ <fill_type>: <color> @ <opacity>

✏️ Border
   Stroke: <color> @ <opacity>, <stroke_weight>px <stroke_align>
   Radius: <uniform>px
   — or —
   Radius: TL <tl>px / TR <tr>px / BR <br>px / BL <bl>px

📦 Layout
   Mode: <HORIZONTAL|VERTICAL>
   Main axis: <primary_axis_align>
   Cross axis: <counter_axis_align>

📏 Spacing
   Padding: <top> <right> <bottom> <left>
   Gap: <item_spacing>px

✨ Effects
   ├─ <type>: <color> @ <opacity>, radius <radius>px, offset (<x>, <y>)
   └─ <type>: radius <radius>px

🔤 Typography (only for TEXT nodes)
   Font: <font_family> <font_weight>
   Size: <font_size>px
   Line height: <line_height_px>px
   Letter spacing: <letter_spacing>px
   Align: <text_align_horizontal> / <text_align_vertical>
   Content: "<characters>"

🧩 Component
   Component ID: <component_id>

👶 Children (<children_count> total)
   ├─ 🖼 <child_name> (<child_type>) — <width>×<height>
   ├─ 🔤 <child_name> (TEXT) — <width>×<height>
   ├─ 🧩 <child_name> (INSTANCE) — <width>×<height> [component: <component_id>]
   └─ 🖼 <child_name> (<child_type>) — <width>×<height>
```

**Formatting rules:**

- **Header line:** Always show name, type, and dimensions. Include the Figma URL on the next line.
- **Sections:** Only include sections where data is present. Omit sections entirely if all values are null.
- **Background/Strokes:** Show each fill/stroke on its own line with tree connectors. For SOLID fills, show `#RRGGBB @ NN%`. For IMAGE or GRADIENT fills, show the type and any available metadata.
- **Border radius:** If uniform (single `cornerRadius`), show one value. If per-corner (`rectangleCornerRadii`), show all four values labeled TL/TR/BR/BL.
- **Spacing:** Show padding as `top right bottom left` (CSS order). Show gap only if `itemSpacing` is set.
- **Typography:** Only shown for TEXT nodes. Include the text content in quotes, truncated to 200 characters with `…` if longer.
- **Children:** Show one level deep with name, type, and dimensions. Use 🖼 for FRAME/RECTANGLE/ELLIPSE/LINE, 🔤 for TEXT, 🧩 for COMPONENT/INSTANCE. If more than 25 children, show the first 20 and note `… and N more`.
- **Component:** Show the `componentId` if present, identifying this node as a component instance.
- **Tree connectors:** Use `├─` for all items except the last, `└─` for the last item.

### Step 7 — Handle errors

This tool uses the standard error handling from the Error Handling section for all API errors. Additional tool-specific errors:

| Condition | Message |
|-----------|---------|
| File key extraction fails | "Could not extract Figma file key. Expected a URL like `figma.com/design/ABC123/...` or a raw file key like `ABC123`." |
| Node ID not provided | "Missing required parameter `node_id`. Provide the Figma node ID to inspect (e.g. `1:2`). Use `figma.file_map` to discover node IDs." |
| Node not found in response | "Node `<NODE_ID>` not found in file `<FILE_KEY>`. Use `figma.file_map` to discover valid node IDs for this file." |
| Node is a DOCUMENT type | "Cannot inspect the root DOCUMENT node. Provide a specific frame or element node ID. Use `figma.file_map` to find frame IDs." |
| Node is a PAGE/CANVAS type | "Inspecting a PAGE node returns limited properties. For detailed frame properties, inspect a specific frame within the page. Use `figma.file_map` to find frame IDs." |
| jq transform fails | "⚠️ jq transform failed for this node. Showing raw node data. The node type `<type>` may have an unexpected structure." |
| Response missing `.nodes` | "Unexpected response structure — `.nodes` field is missing. Verify the file key and node ID are correct." |

## Tool: figma.export

Exports one or more Figma nodes as image files (PNG or SVG). Calls the Figma Images API to render the nodes, downloads the resulting files to local workspace storage, and returns the local file paths. Use these paths with other tools — attach to Slack messages, embed in PR descriptions, or reference in Linear tickets.

### Input

- `file_key` (required) — Figma file key OR full Figma URL. If a URL is provided, the file key is extracted automatically.
- `node_ids` (required) — Comma-separated list of node IDs to export (e.g. `1:2` or `1:2,3:4,5:6`). Use `figma.file_map` to discover node IDs.
- `format` (optional) — Export format: `png` or `svg`. Default: `png`.
- `scale` (optional) — Image scale factor for PNG exports: `1`, `2`, `3`, or `4`. Default: `2` (retina-ready). Ignored for SVG exports (vector format, scale does not apply).

### Step 1 — Parse the file key

If the input contains `figma.com`, extract the file key from the URL path:

```bash
FILE_KEY=$(echo "<input>" | sed -n 's|.*figma\.com/\(design\|file\|board\)/\([a-zA-Z0-9]*\).*|\2|p')
```

If the input does not contain `figma.com`, treat the entire input as the file key:

```bash
FILE_KEY="<input>"
```

**Validate** the file key:
- Must be non-empty
- Must contain only alphanumeric characters (a-z, A-Z, 0-9)

If validation fails, **STOP** and report:

> Could not extract Figma file key. Expected a URL like `figma.com/design/ABC123/...` or a raw file key like `ABC123`.

### Step 2 — Retrieve the token

```
vault.get: path=skills/figma/token
```

If the token is unavailable, follow the error handling in the Authentication section.

### Step 3 — Validate and normalize inputs

**Validate node IDs:**
- `node_ids` must be non-empty.
- Split on commas and trim whitespace from each entry.
- Each node ID must match the pattern `N:N` (digits, colon, digits). If any ID is malformed, **STOP** and report:

> Invalid node ID `<ID>`. Node IDs use the format `N:N` (e.g. `1:2`). Use `figma.file_map` to discover valid node IDs.

**Normalize format:**
- Convert `format` to lowercase.
- If not `png` or `svg`, **STOP** and report:

> Unsupported format `<format>`. Supported formats: `png`, `svg`.

**Normalize scale:**
- If format is `svg`, ignore the scale parameter entirely (SVG is a vector format).
- If format is `png`, validate scale is one of `1`, `2`, `3`, `4`. If not, **STOP** and report:

> Invalid scale `<scale>`. Supported PNG scales: 1, 2, 3, 4.

### Step 4 — Request renders from Figma

Enforce rate limiting — if this is not the first Figma API call in the session, wait at least 2 seconds:

```
exec.run: sleep 2
```

URL-encode the node IDs: replace `:` with `%3A` and join with `,` (e.g. `1:2,3:4` becomes `1%3A2,3%3A4`).

Build the API URL based on format:

**For PNG:**

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/images/<FILE_KEY>?ids=<ENCODED_IDS>&format=png&scale=<SCALE>"
```

**For SVG:**

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/images/<FILE_KEY>?ids=<ENCODED_IDS>&format=svg"
```

Log the request per the Logging section:

```
→ GET /v1/images/<FILE_KEY>?ids=<ENCODED_IDS>&format=<FORMAT>&scale=<SCALE> ...
```

Parse the response: split on the last newline to separate the JSON body from the HTTP status code.

- **200** — Continue to Step 5.
- **400** — "Bad request — check that node IDs are valid. Use `figma.file_map` to discover valid node IDs."
- **404** — "File not found — verify the file key `<FILE_KEY>` exists and your token has access."
- **Any other non-200 status** — Apply the standard error handling from the Error Handling section.

Log the response:

```
← <status_code> (<response_size> bytes)
```

**Parse the response body** using jq:

```bash
echo '<RESPONSE_BODY>' | jq -r '.images | to_entries[] | "\(.key)\t\(.value)"'
```

This produces tab-separated lines of `node_id\timage_url`. If `.images` is null or empty, **STOP** and report:

> Figma returned no image URLs. The file may have no renderable content for the specified nodes.

Check the `.err` field:

```bash
echo '<RESPONSE_BODY>' | jq -r '.err // empty'
```

If `.err` is non-null and non-empty, log a warning but continue — some nodes may still have valid URLs:

```
⚠️ Figma reported an error: <err>. Some nodes may not have been rendered.
```

### Step 5 — Create output directory

```
exec.run: mkdir -p .local/figma-exports/<FILE_KEY>/
```

### Step 6 — Download images

For each entry in the parsed `.images` response (node ID → URL pairs):

1. **Skip null URLs** — If the URL for a node is `null`, record it as a failure and continue to the next node:

   ```
   ✗ Node <NODE_ID>: Figma returned no render URL (node may not be renderable)
   ```

2. **Sanitize the node ID** for use as a filename — replace `:` with `-` (e.g. `1:2` becomes `1-2`):

   ```bash
   SAFE_ID=$(echo "<NODE_ID>" | tr ':' '-')
   ```

3. **Download the image** with a 60-second timeout:

   ```
   exec.run: curl -sL --max-time 60 -o .local/figma-exports/<FILE_KEY>/<SAFE_ID>.<FORMAT> "<IMAGE_URL>"
   ```

4. **Enforce rate limiting** — wait 2 seconds before the next download:

   ```
   exec.run: sleep 2
   ```

Track successes and failures separately as you go.

### Step 7 — Verify downloads

For each downloaded file, verify it exists and has non-zero size:

```bash
exec.run: wc -c < .local/figma-exports/<FILE_KEY>/<SAFE_ID>.<FORMAT>
```

If the file does not exist or has zero bytes, move it from the success list to the failure list:

```
✗ Node <NODE_ID>: download produced an empty file
```

### Step 8 — Format output

**If all exports succeeded:**

```
📦 Exported <N> asset(s) to .local/figma-exports/<FILE_KEY>/

  ├─ 🖼 <SAFE_ID>.<FORMAT> (<FILE_SIZE>) — node <NODE_ID>
  ├─ 🖼 <SAFE_ID>.<FORMAT> (<FILE_SIZE>) — node <NODE_ID>
  └─ 🖼 <SAFE_ID>.<FORMAT> (<FILE_SIZE>) — node <NODE_ID>
```

**If some exports failed (partial success):**

```
📦 Exported <N_SUCCESS> of <N_TOTAL> asset(s) to .local/figma-exports/<FILE_KEY>/

✅ Succeeded:
  ├─ 🖼 <SAFE_ID>.<FORMAT> (<FILE_SIZE>) — node <NODE_ID>
  └─ 🖼 <SAFE_ID>.<FORMAT> (<FILE_SIZE>) — node <NODE_ID>

❌ Failed:
  ├─ Node <NODE_ID>: <failure reason>
  └─ Node <NODE_ID>: <failure reason>
```

**If all exports failed:**

```
❌ Export failed — no assets were downloaded.

  ├─ Node <NODE_ID>: <failure reason>
  └─ Node <NODE_ID>: <failure reason>
```

**Formatting rules:**

- **File size:** Format as human-readable: bytes for <1 KB, KB with one decimal for <1 MB, MB with one decimal for ≥1 MB.
- **Tree connectors:** Use `├─` for all items except the last, `└─` for the last item.
- **Icon:** Use 🖼 for all exported image files regardless of format.

### Step 9 — Handle errors

This tool uses the standard error handling from the Error Handling section for all API errors. Additional tool-specific errors:

| Condition | Message |
|-----------|---------|
| File key extraction fails | "Could not extract Figma file key. Expected a URL like `figma.com/design/ABC123/...` or a raw file key like `ABC123`." |
| No node IDs provided | "Missing required parameter `node_ids`. Provide one or more comma-separated Figma node IDs to export (e.g. `1:2` or `1:2,3:4`). Use `figma.file_map` to discover node IDs." |
| Invalid node ID format | "Invalid node ID `<ID>`. Node IDs use the format `N:N` (e.g. `1:2`). Use `figma.file_map` to discover valid node IDs." |
| Unsupported format | "Unsupported format `<format>`. Supported formats: `png`, `svg`." |
| Invalid scale | "Invalid scale `<scale>`. Supported PNG scales: 1, 2, 3, 4." |
| Figma returns empty `.images` | "Figma returned no image URLs. The file may have no renderable content for the specified nodes." |
| Node URL is null | "Node `<NODE_ID>`: Figma returned no render URL (node may not be renderable)." |
| Download times out | "Node `<NODE_ID>`: download timed out after 60 seconds. The rendered image may be very large — try exporting at a lower scale or as SVG." |
| Download produces empty file | "Node `<NODE_ID>`: download produced an empty file. The render URL may have expired — retry the export." |
| All downloads fail | "Export failed — no assets were downloaded. Check that the node IDs are valid and renderable." |

## Tool: figma.brief

Generates a structured implementation brief from a Figma frame. Takes a file key and frame node ID, fetches the full node subtree and a visual reference PNG, then walks the tree recursively to extract design tokens — colours, typography, layout, dimensions, and component instances. Output is opinionated Markdown formatted as a developer specification, not a raw data dump.

### Input

- `file_key` (required) — Figma file key OR full Figma URL. If a URL is provided, the file key is extracted automatically.
- `node_id` (required) — Frame node ID to brief (e.g. `1:2`). Use `figma.file_map` to discover node IDs.
- `sub_node_id` (optional) — Target a specific sub-frame within the main frame. If provided, the brief is scoped to only that subtree.

### Step 1 — Parse the file key

If the input contains `figma.com`, extract the file key from the URL path:

```bash
FILE_KEY=$(echo "<input>" | sed -n 's|.*figma\.com/\(design\|file\|board\)/\([a-zA-Z0-9]*\).*|\2|p')
```

If the input does not contain `figma.com`, treat the entire input as the file key:

```bash
FILE_KEY="<input>"
```

**Validate** the file key:
- Must be non-empty
- Must contain only alphanumeric characters (a-z, A-Z, 0-9)

If validation fails, **STOP** and report:

> Could not extract Figma file key. Expected a URL like `figma.com/design/ABC123/...` or a raw file key like `ABC123`.

### Step 2 — Retrieve the token

```
vault.get: path=skills/figma/token
```

If the token is unavailable, follow the error handling in the Authentication section.

### Step 3 — Export visual reference

Enforce rate limiting — if this is not the first Figma API call in the session, wait at least 2 seconds:

```
exec.run: sleep 2
```

URL-encode the node ID: replace `:` with `%3A` (e.g. `1:2` becomes `1%3A2`).

Request a 2x PNG render from the Figma Images API:

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/images/<FILE_KEY>?ids=<ENCODED_NODE_ID>&format=png&scale=2"
```

Log the request:

```
→ GET /v1/images/<FILE_KEY>?ids=<NODE_ID>&format=png&scale=2 ...
```

Parse the response: split on the last newline to separate the JSON body from the HTTP status code.

- **200** — Extract the image URL from `.images["<NODE_ID>"]`.
- **Any non-200 status** — Apply the standard error handling from the Error Handling section.

If the image URL is not null, download it:

```bash
SAFE_ID=$(echo "<NODE_ID>" | tr ':' '-')
exec.run: mkdir -p .local/figma-exports/<FILE_KEY>
exec.run: curl -sL --max-time 60 -o .local/figma-exports/<FILE_KEY>/<SAFE_ID>.png "<IMAGE_URL>"
```

Verify the download:

```
exec.run: wc -c < .local/figma-exports/<FILE_KEY>/<SAFE_ID>.png
```

Set `IMAGE_PATH=.local/figma-exports/<FILE_KEY>/<SAFE_ID>.png` on success.

**If the export fails at any point** (non-200 status, null URL, empty download), set `IMAGE_PATH=null` and continue to Step 4. The brief will note "Visual reference unavailable" instead of a file path. Do **not** stop the procedure for an export failure.

### Step 4 — Fetch full node subtree

Enforce rate limiting:

```
exec.run: sleep 2
```

Fetch the node with its complete subtree (no depth limit):

```
exec.run: curl -s -w "\n%{http_code}" --max-time 30 -H "X-Figma-Token: <TOKEN>" "https://api.figma.com/v1/files/<FILE_KEY>/nodes?ids=<ENCODED_NODE_ID>"
```

Log the request:

```
→ GET /v1/files/<FILE_KEY>/nodes?ids=<NODE_ID> ...
```

Parse the response: split on the last newline to separate the JSON body from the HTTP status code.

- **200** — Continue to Step 5.
- **404** — "Node or file not found — verify the file key `<FILE_KEY>` and node ID `<NODE_ID>` are correct. Use `figma.file_map` to discover valid node IDs."
- **Any other non-200 status** — Apply the standard error handling from the Error Handling section.

Extract the node document:

```bash
NODE_DOC=$(echo '<RESPONSE_BODY>' | jq --arg nid "<NODE_ID>" '.nodes[$nid].document')
```

If `NODE_DOC` is `null` or empty, **STOP** and report:

> Node `<NODE_ID>` not found in file `<FILE_KEY>`. Use `figma.file_map` to discover valid node IDs for this file.

### Step 5 — Extract design tokens with jq

If `sub_node_id` is provided, first filter the tree to that subtree:

```bash
NODE_DOC=$(echo '<NODE_DOC>' | jq --arg sid "<SUB_NODE_ID>" '
  def find_node:
    if .id == $sid then .
    elif .children then (.children[] | find_node) // null
    else null end;
  find_node
')
```

If the filtered result is `null`, **STOP** and report:

> Sub-node `<SUB_NODE_ID>` not found within node `<NODE_ID>`. Use `figma.inspect` on node `<NODE_ID>` to see its children and their IDs.

Pipe the node document through `jq` to recursively walk the tree and extract all design tokens. The recursive walk is capped at 10 levels of nesting.

```bash
echo '<NODE_DOC>' | jq --arg fk "<FILE_KEY>" '
def to_hex:
  def h: if . < 10 then . + 48 else . + 87 end;
  [(. / 16 | floor | h), (. % 16 | h)] | implode;

def rgb_to_hex:
  "#" + ((.r * 255 | floor) | to_hex) +
        ((.g * 255 | floor) | to_hex) +
        ((.b * 255 | floor) | to_hex);

def collect_hierarchy(depth):
  if depth > 10 then { name: .name, type: .type, id: .id, truncated: true }
  else {
    name: .name,
    type: .type,
    id: .id,
    dimensions: (if .absoluteBoundingBox then {
      width: (.absoluteBoundingBox.width | round),
      height: (.absoluteBoundingBox.height | round)
    } else null end),
    children: [(.children // [])[] | collect_hierarchy(depth + 1)]
  } end;

def collect_colours(context):
  [(.fills // [])[] | select(.visible != false and .type == "SOLID" and .color) |
    { hex: (.color | rgb_to_hex), opacity: ((.color.a // 1) * 100 | round), usage: context }],
  [(.strokes // [])[] | select(.visible != false and .type == "SOLID" and .color) |
    { hex: (.color | rgb_to_hex), opacity: ((.color.a // 1) * 100 | round), usage: "stroke" }];

def collect_all_colours(depth):
  if depth > 10 then []
  else
    (collect_colours(.name)) +
    [(.children // [])[] | collect_all_colours(depth + 1)] | flatten
  end;

def collect_text_styles(depth):
  if depth > 10 then []
  else
    (if .type == "TEXT" and .style then [{
      font_family: (.style.fontFamily // "unknown"),
      font_weight: (.style.fontWeight // 400),
      font_size: (.style.fontSize // 0),
      line_height_px: (.style.lineHeightPx // null),
      letter_spacing: (.style.letterSpacing // 0),
      colour: (if (.fills // [])[0].color then
        ((.fills // [])[0].color | rgb_to_hex)
      else null end)
    }] else [] end) +
    [(.children // [])[] | collect_text_styles(depth + 1)] | flatten
  end;

def collect_layouts(depth):
  if depth > 10 then []
  else
    (if .layoutMode then [{
      name: .name,
      mode: .layoutMode,
      primary_axis_align: (.primaryAxisAlignItems // null),
      counter_axis_align: (.counterAxisAlignItems // null),
      padding: [(.paddingTop // 0), (.paddingRight // 0), (.paddingBottom // 0), (.paddingLeft // 0)],
      item_spacing: (.itemSpacing // 0),
      layout_wrap: (.layoutWrap // null)
    }] else [] end) +
    [(.children // [])[] | collect_layouts(depth + 1)] | flatten
  end;

def collect_components(depth):
  if depth > 10 then []
  else
    (if .type == "INSTANCE" and .componentId then [{
      name: .name,
      component_id: .componentId
    }] else [] end) +
    [(.children // [])[] | collect_components(depth + 1)] | flatten
  end;

def collect_dimensions(depth):
  if depth > 10 then []
  else
    (if .absoluteBoundingBox and (.type == "FRAME" or .type == "COMPONENT" or .type == "INSTANCE" or .type == "SECTION") then [{
      name: .name,
      width: (.absoluteBoundingBox.width | round),
      height: (.absoluteBoundingBox.height | round)
    }] else [] end) +
    [(.children // [])[] | collect_dimensions(depth + 1)] | flatten
  end;

{
  name: .name,
  type: .type,
  id: .id,
  url: ("https://www.figma.com/design/" + $fk + "?node-id=" + (.id | gsub(":"; "-"))),
  dimensions: (if .absoluteBoundingBox then {
    width: (.absoluteBoundingBox.width | round),
    height: (.absoluteBoundingBox.height | round)
  } else null end),
  hierarchy: collect_hierarchy(0),
  colours: (collect_all_colours(0) | group_by(.hex) | map({
    hex: .[0].hex,
    opacity: .[0].opacity,
    usages: [.[].usage] | unique
  })),
  text_styles: (collect_text_styles(0) | unique_by([.font_family, .font_weight, .font_size])),
  layouts: collect_layouts(0),
  components: (collect_components(0) | group_by(.name) | map({
    name: .[0].name,
    component_id: .[0].component_id,
    count: length
  })),
  key_dimensions: collect_dimensions(0)
}
'
```

If the jq transform fails, fall back to a top-level-only extraction with a warning:

```bash
echo '<NODE_DOC>' | jq --arg fk "<FILE_KEY>" '{
  name: .name,
  type: .type,
  id: .id,
  url: ("https://www.figma.com/design/" + $fk + "?node-id=" + (.id | gsub(":"; "-"))),
  dimensions: (if .absoluteBoundingBox then {
    width: (.absoluteBoundingBox.width | round),
    height: (.absoluteBoundingBox.height | round)
  } else null end),
  children_count: ((.children // []) | length),
  fallback: true
}'
```

Report:

> ⚠️ Recursive extraction failed for this node. Generating brief from top-level properties only. The node type `<type>` may have a complex structure — try using `figma.inspect` for detailed per-node analysis.

### Step 6 — Assemble the Markdown brief

Using the extracted JSON data, assemble the implementation brief. Each section is built only if the corresponding data is non-empty.

#### Header

```
# Implementation Brief: {name}

📐 Dimensions: {width}×{height} | Visual: {IMAGE_PATH or "unavailable"}
Figma: {url}
```

If `IMAGE_PATH` is null (export failed in Step 3), display:

```
📐 Dimensions: {width}×{height} | Visual: ⚠️ unavailable (export failed)
Figma: {url}
```

#### Component Hierarchy

Render the `hierarchy` object as an indented tree. Use 2-space indentation per level with tree connectors:

```
## Component Hierarchy

├─ 🖼 Frame: Header (320×60)
│  ├─ 🔤 Text: Title
│  ├─ 🧩 Instance: Avatar
│  └─ 🖼 Frame: Actions (120×40)
│     ├─ 🧩 Instance: Button
│     └─ 🧩 Instance: Button
└─ 🖼 Frame: Content (320×400)
   ├─ 🔤 Text: Description
   └─ 🖼 Frame: Card List (320×350)
```

**Icon mapping:**
- `FRAME`, `RECTANGLE`, `ELLIPSE`, `LINE`, `VECTOR`, `GROUP` → 🖼
- `TEXT` → 🔤
- `COMPONENT`, `INSTANCE`, `COMPONENT_SET` → 🧩
- `SECTION` → 📂

**Tree rules:**
- Use `├─` for all items except the last child at each level, `└─` for the last child.
- Use `│  ` (pipe + 2 spaces) for continuation lines.
- Show dimensions in parentheses for FRAME, COMPONENT, INSTANCE, and SECTION nodes.
- If a branch was truncated at the depth cap, append `… (truncated at depth 10)`.

#### Layout Specification

Render the `layouts` array as a specification list:

```
## Layout Specification

**Header** — Horizontal, gap: 12px
  Align: CENTER / CENTER
  Padding: 16 16 16 16

**Content** — Vertical, gap: 8px
  Align: MIN / MIN
  Padding: 24 16 24 16
  Wrap: NO_WRAP

**Card List** — Vertical, gap: 12px
  Align: MIN / STRETCH
  Padding: 0 0 0 0
```

**Formatting rules:**
- Bold the container name.
- Show direction as "Horizontal" (for `HORIZONTAL`) or "Vertical" (for `VERTICAL`).
- Show alignment as `primary_axis_align / counter_axis_align`.
- Show padding as `top right bottom left` (CSS order).
- Show gap as `item_spacing` with `px` suffix.
- Only include `Wrap:` line if `layout_wrap` is not null.

#### Typography

Render the `text_styles` array as a Markdown table:

```
## Typography

| Font | Size | Weight | Line Height | Letter Spacing | Colour |
|------|------|--------|-------------|----------------|--------|
| Inter | 24px | 700 | 32px | 0px | #1a1a1a |
| Inter | 16px | 400 | 24px | 0px | #333333 |
| Inter | 14px | 400 | 20px | 0.2px | #666666 |
```

**Formatting rules:**
- Deduplicate by font family + weight + size (already done in jq).
- Sort by font size descending (largest first).
- Show line height as `Npx` or `—` if null.
- Show letter spacing as `Npx`.
- Show colour as hex or `—` if null.

#### Colour Palette

Render the `colours` array as a Markdown table:

```
## Colour Palette

| Swatch | Hex | Opacity | Usage |
|--------|-----|---------|-------|
| 🟦 | #1a73e8 | 100% | Button, Link |
| ⬛ | #1a1a1a | 100% | Title, Heading |
| ⬜ | #ffffff | 100% | Background, Card |
| 🔲 | #e0e0e0 | 100% | stroke |
| 🟫 | #666666 | 80% | Caption |
```

**Swatch mapping** (by rough brightness/hue of hex value):
- Default: 🔲
- Very dark (#000000–#333333): ⬛
- Very light (#eeeeee–#ffffff): ⬜
- Blue-ish hue: 🟦
- Red-ish hue: 🟥
- Green-ish hue: 🟩
- Yellow/orange hue: 🟨
- Purple-ish hue: 🟪
- Brown/earth tones: 🟫

If exact hue mapping is ambiguous, use 🔲.

**Formatting rules:**
- Show opacity as percentage (e.g. `100%`, `80%`).
- Capitalise the first letter of each usage context.
- Join multiple usages with `, `.

#### Key Dimensions

Render the `key_dimensions` array as a list:

```
## Key Dimensions

- **Root frame:** 375×812
- **Header:** 375×60
- **Content:** 375×400
- **Card:** 343×120
- **Button:** 120×44
```

**Formatting rules:**
- Bold the component name.
- Show dimensions as `width×height`.
- List the root frame first, then children sorted by area (largest first).
- Maximum 15 entries — if more, show the 15 largest and note `… and N more containers`.

#### Component Inventory

Render the `components` array as a list:

```
## Component Inventory

| Component | Instances | Component ID |
|-----------|-----------|--------------|
| Button | 3 | 1:234 |
| Avatar | 2 | 1:567 |
| Icon | 5 | 1:890 |
| Card | 4 | 1:345 |
```

**Formatting rules:**
- Sort by instance count descending.
- Show the component ID for cross-referencing with the Figma file.
- If there are no component instances, omit this section entirely.

### Step 7 — Format output

Present the complete assembled brief to the user. The brief is the primary output — present it as the direct response, not wrapped in a code block.

After the brief, add a separator and a summary line:

```
---

📋 Brief generated from `{node_id}` in file `{FILE_KEY}`
Visual: `{IMAGE_PATH}` (or "unavailable")
Sections: {list of sections included}
Extracted: {N} colours, {N} text styles, {N} layout containers, {N} component types
```

### Step 8 — Handle errors

This tool uses the standard error handling from the Error Handling section for all API errors. Additional tool-specific errors:

| Condition | Message |
|-----------|---------|
| File key extraction fails | "Could not extract Figma file key. Expected a URL like `figma.com/design/ABC123/...` or a raw file key like `ABC123`." |
| Node ID not provided | "Missing required parameter `node_id`. Provide the Figma frame node ID to brief (e.g. `1:2`). Use `figma.file_map` to discover node IDs." |
| Node not found in response | "Node `<NODE_ID>` not found in file `<FILE_KEY>`. Use `figma.file_map` to discover valid node IDs for this file." |
| Sub-node not found | "Sub-node `<SUB_NODE_ID>` not found within node `<NODE_ID>`. Use `figma.inspect` on node `<NODE_ID>` to see its children and their IDs." |
| Node is a DOCUMENT type | "Cannot brief the root DOCUMENT node. Provide a specific frame or component node ID. Use `figma.file_map` to find frame IDs." |
| Export fails | Continue generating the brief. Note "Visual: ⚠️ unavailable (export failed)" in the header. Do not stop. |
| Frame has no children | Generate a brief with only the root frame's own properties: dimensions, background colour, border radius. Note "This frame has no child elements." below the header. |
| jq recursive walk fails | Fall back to top-level-only extraction. Include warning: "⚠️ Recursive extraction failed — brief generated from top-level properties only." |
| Very deep nesting (>10 levels) | Tree is automatically capped at 10 levels. Note "… (truncated at depth 10)" at the truncation point. |

## Testing

To verify this skill works correctly, use the known test file:

- **File key:** `EXAMPLEfileKey00000000`
- **URL form:** `https://www.figma.com/design/EXAMPLEfileKey00000000/Example`

### Test cases

#### figma.file_map

1. **Raw file key:** Call `figma.file_map` with `file_key=EXAMPLEfileKey00000000`. Expect a structured tree with pages and frames.
2. **Full URL:** Call with `file_key=https://www.figma.com/design/EXAMPLEfileKey00000000/Example`. Expect the same result.
3. **Page filter:** Call with `page_filter=<name of a known page>`. Expect only matching pages in output.
4. **Invalid file key:** Call with `file_key=xxx`. Expect a 404 error with actionable message.
5. **Invalid URL:** Call with `file_key=https://example.com/not-figma`. Expect the extraction error message.

#### figma.inspect

6. **Frame inspection:** Call `figma.inspect` with `file_key=EXAMPLEfileKey00000000` and `node_id=<valid frame ID from file_map>`. Expect structured output with dimensions, fills, layout properties, and children list.
7. **Text node inspection:** Call `figma.inspect` with a TEXT node ID. Expect the Typography section with font family, weight, size, line height, letter spacing, and text content.
8. **Invalid node ID:** Call with `file_key=EXAMPLEfileKey00000000` and `node_id=999:999`. Expect a clear error: "Node `999:999` not found in file..."
9. **Component inspection:** Call `figma.inspect` with a COMPONENT or INSTANCE node ID. Expect the Component section with the component ID and variant name if applicable.
10. **Full URL with node:** Call with `file_key=https://www.figma.com/design/EXAMPLEfileKey00000000/Example` and a valid `node_id`. Expect the same result as using the raw file key.

#### figma.export

11. **PNG export:** Call `figma.export` with `file_key=EXAMPLEfileKey00000000`, `node_ids=<valid frame ID from file_map>`, and `format=png`. Expect a PNG file downloaded to `.local/figma-exports/EXAMPLEfileKey00000000/<node_id>.png` with non-zero file size. Default scale should be 2x.
12. **SVG export:** Call `figma.export` with the same file key and node ID but `format=svg`. Expect an SVG file at `.local/figma-exports/EXAMPLEfileKey00000000/<node_id>.svg`. Verify the scale parameter is ignored for SVG.
13. **Custom scale:** Call with `format=png` and `scale=1`. Expect a PNG file. Call again with `scale=3`. Expect a larger file than scale 1.
14. **Batch export:** Call `figma.export` with `node_ids=<id1>,<id2>,<id3>` (multiple valid frame IDs). Expect all files downloaded with individual success entries in the output.
15. **Invalid node ID:** Call with `node_ids=999:999`. Expect either a null URL from Figma or a clear failure message — not a crash.
16. **Invalid file key:** Call with `file_key=xxx` and a valid node ID. Expect a 404 error with actionable message.
17. **Partial failure:** Call with `node_ids=<valid_id>,999:999` (one valid, one invalid). Expect partial success output: the valid node's file is downloaded and the invalid node is listed as failed.
18. **Full URL:** Call with `file_key=https://www.figma.com/design/EXAMPLEfileKey00000000/Example` and valid `node_ids`. Expect the same result as using the raw file key.

#### figma.brief

19. **Basic brief:** Call `figma.brief` with `file_key=EXAMPLEfileKey00000000` and `node_id=<valid frame ID from file_map>`. Expect a complete Markdown brief with all sections: Component Hierarchy, Layout Specification, Typography, Colour Palette, Key Dimensions, and Component Inventory. Expect a visual reference PNG downloaded to `.local/figma-exports/`. The summary line should list the number of colours, text styles, layout containers, and component types extracted.
20. **Sub-node targeting:** Call `figma.brief` with `file_key=EXAMPLEfileKey00000000`, `node_id=<valid frame ID>`, and `sub_node_id=<valid child ID within that frame>`. Expect the brief to be scoped to only the sub-frame's subtree — the hierarchy should start at the sub-node, and extracted tokens should only come from that branch.
21. **Complex frame with sections:** Call `figma.brief` on a frame that contains SECTION children. Expect each section to appear as a 📂 node in the Component Hierarchy, with its children nested beneath it.
22. **Brief with full URL:** Call `figma.brief` with `file_key=https://www.figma.com/design/EXAMPLEfileKey00000000/Example` and a valid `node_id`. Expect the same result as using the raw file key.
23. **Invalid node ID:** Call `figma.brief` with `file_key=EXAMPLEfileKey00000000` and `node_id=999:999`. Expect a clear error: "Node `999:999` not found in file..."
24. **Text-heavy frame:** Call `figma.brief` on a frame containing multiple text nodes with different font families, weights, and sizes. Expect the Typography table to contain deduplicated rows sorted by font size descending, with no duplicate entries for the same font/weight/size combination.
25. **Empty frame:** Call `figma.brief` on a frame with no children. Expect a brief with the root frame's own properties (dimensions, background colour) and a note: "This frame has no child elements."
26. **Invalid sub-node:** Call `figma.brief` with valid `file_key` and `node_id` but `sub_node_id=999:999`. Expect a clear error: "Sub-node `999:999` not found within node..."
