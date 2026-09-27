# `surface-ctl` — the surfaced control protocol

**Status:** v0.1 (surfaced daemon, 2026-09-27) — implemented by `crates/surfaced`.
**Companion:** [`CONTROL-PROTOCOL.md`](CONTROL-PROTOCOL.md) §1 (the **shared framing** —
Unix socket, NDJSON, JSON-RPC 2.0, the `hello` gate + versioning, local trust),
[`DESIGN.md`](DESIGN.md) §10.1 (the browser **surface** capability), §4 (plugins drive
data-plane daemons over control sockets).

`surface-ctl` is the **local seam** between capmeshd's in-process `surface` adapter
(DESIGN §4) and the **`surfaced`** data-plane daemon, which owns the durable surface
store, the HTTP/SSE attachment serving, and attachment fan-out. capmeshd stays stateless
plumbing and drives `surfaced` over this socket; it carries no surface data itself.

> **Control plane vs. data plane — a load-bearing split (operator directive).**
> capmeshd is **control plane + service discovery only**: it advertises/discovers
> surface capabilities on the mesh, wires topology, and reports status — it **never
> pushes items or handles surface content**. **All data interaction** — serving the
> surface, the durable inbox, and **pushing display items** — is `surfaced`'s job (the
> data daemon), exposed over its **HTTP/SSE** surface and its **embedded MCP server**
> (§8), which an agent talks to **directly**. Over `surface-ctl` capmeshd uses the
> **control** methods — register a surface (`create-surface`), discover
> (`list-surfaces`), inspect (`list-items`), lifecycle. Content push (`send-item`) and
> view control (`set-view`) are **data-plane** operations: the primary path is
> `surfaced`'s HTTP/MCP, and they are offered on the local socket only as a co-located
> convenience — **capmeshd does not push content**.

This is a **sibling** of the mount protocol in `CONTROL-PROTOCOL.md`, not a section of
it: the two share only the **framing** (§1 there). `capmesh-ctl` proper is the typed
port + mount/unmount/negotiation model for streaming data planes (nmidid, audio, …);
`surface-ctl` is a **distinct RPC surface** (create-surface / send-item / set-view /
list-items) over the same wire. Keeping it here lets the frozen mount contract stay
stable while this protocol evolves independently.

---

## 1. Transport & framing — shared with `capmesh-ctl`

The transport, framing, JSON-RPC semantics, `hello` handshake gate, versioning, and
local-trust model are **exactly as specified in [`CONTROL-PROTOCOL.md`](CONTROL-PROTOCOL.md)
§1**. In brief (see there for the normative text):

- **Unix domain socket**, local only. `surfaced` binds it when started with `--socket`
  (`SURFACED_SOCKET`); the default deployment path is `/run/surfaced/surfaced.sock`.
  Without `--socket`, surfaced runs HTTP-only and this protocol is unavailable.
- **NDJSON** framing — one JSON value per line.
- **JSON-RPC 2.0** — requests carry an `id`; the daemon replies with a matching `result`
  or `error`. capmeshd is the client; `surfaced` is the server.
- **`hello` first (§1.2 there).** The first request MUST be `hello`; every other method is
  refused with `not-ready` until a compatible `hello` completes. `protocol` is a single
  integer major version; an unknown major is refused with `unsupported-protocol`.
- **Local trust (§1.1 there).** The socket is owner/group read-write (`0o660`); it carries
  no cluster key. The cluster credential (DESIGN §8) authenticates the *inter-host* mesh,
  not this socket. Because this socket is local-trust, it is also the **only** place a
  surface's attach token is set (§4) — never over HTTP.

### 1.1 Handshake

```jsonc
// → request
{"jsonrpc":"2.0","id":1,"method":"hello",
 "params":{"protocol":"1","client":"capmeshd/0.1"}}
// ← result
{"jsonrpc":"2.0","id":1,"result":{
   "protocol":"1", "daemon":"surfaced/0.1.0",
   "capabilities":["surface","durable-inbox","attach-fanout"]}}
```

`capabilities` advertises what this daemon supports: `surface` (the surface capability
kind), `durable-inbox` (each surface persists its inbox on disk, replayed on restart),
`attach-fanout` (live attachments receive pushes over SSE).

---

## 2. The surface model on the wire

A **surface** is a durable, ordered, bounded **inbox** (DESIGN §10.1): every push appends
a **display item**; the head (latest promoted item) is the **main view**. State lives in
`surfaced`, not in a browser tab, and persists whether or not a tab is attached.

A **display item** is the payload of `send-item`, tagged by `type` (the built-in item
types, §4):

```jsonc
{"type": "pdf", "url": "https://host/manual.pdf"}
```

An **inbox item** is what `list-items` returns — a display item plus its identity, push
time, and whether it promoted to the main view:

```jsonc
{"id": "b1f0…", "ts": 1790538548069, "promote": true,
 "item": {"type": "pdf", "url": "https://host/manual.pdf"}}
```

A **surface view** (the `list-items` result) is the surface's current state:

```jsonc
{"id": "phone", "title": "My Phone", "current-view": "b1f0…",
 "items": [ /* InboxItem, oldest-first, bounded to the recent window */ ]}
```

---

## 3. Methods (capmeshd → surfaced)

| method | params | result | purpose |
|---|---|---|---|
| `hello` | `{protocol, client}` | `{protocol, daemon, capabilities}` | handshake (§1.1) |
| `create-surface` | `{surface-id, title?, attach-token?}` | `{id}` | ensure a surface exists; optionally set its title + attach token |
| `set-token` | `{surface-id, attach-token?}` | `{}` | rotate (string) or clear (null/absent → reopen) an existing surface's attach token; `no-such-surface` if it does not exist |
| `send-item` | `{surface-id, item, promote?}` | `{id, ts}` | append a display item to the inbox (and, if `promote`, make it the main view) |
| `set-view` | `{surface-id, item-id?}` | `{}` | select which item the main view shows (`item-id` null/absent clears it) |
| `remove-item` | `{surface-id, item-id}` | `{}` | prune one item from the inbox by id; `no-such-item` if the surface or item is unknown |
| `list-items` | `{surface-id}` | `SurfaceView` | the surface's current state (title, main view, items) |
| `list-surfaces` | `{}` | `{surfaces: [SurfaceSummary]}` | discover every registered surface (id, title, item-count, current-view) — the control/discovery method |

The **control/discovery** methods (`create-surface`, `list-surfaces`, `list-items`)
are capmeshd's use of this socket. `send-item` and `set-view` are **data-plane**
operations (see the split above): capmeshd does not push content; an agent uses
`surfaced`'s HTTP/MCP directly, and these socket methods are a co-located convenience.

Every method but `hello` requires a completed `hello`. `surface-id` MUST be a path-safe
id (`[A-Za-z0-9._-]+`, not `.`/`..`, ≤128 chars); otherwise `invalid-surface-id`.

A `send-item` push and an HTTP `POST /s/{id}/items` are **identical** — both append to the
same store and fan out to every attached tab.

### 3.1 `create-surface`

```jsonc
{"jsonrpc":"2.0","id":2,"method":"create-surface",
 "params":{"surface-id":"phone","title":"My Phone","attach-token":"s3cr3t"}}
```

Idempotent: a `null`/absent `title` or `attach-token` leaves the existing value unchanged,
so re-creating a surface never clobbers it. `attach-token` may be set here **only** (local
trust); once set, HTTP attachment to that surface requires the token (§4).

### 3.2 `send-item`

```jsonc
{"jsonrpc":"2.0","id":3,"method":"send-item",
 "params":{"surface-id":"phone","promote":true,
           "item":{"type":"pdf","url":"https://host/manual.pdf"}}}
// ← result
{"jsonrpc":"2.0","id":3,"result":{"id":"b1f0…","ts":1790538548069}}
```

`promote` defaults to `true` (the latest push becomes the main view); `false` appends to
the inbox feed without moving the main view.

### 3.3 `set-view` / `list-items`

```jsonc
{"jsonrpc":"2.0","id":4,"method":"set-view","params":{"surface-id":"phone","item-id":"b1f0…"}}
{"jsonrpc":"2.0","id":5,"method":"list-items","params":{"surface-id":"phone"}}
```

`set-view` with an unknown surface or `item-id` returns `no-such-item`.

---

## 4. Display item types

The built-in item types (DESIGN §10.1). `navigate` is a third-party URL rendered inside a
**sandboxed iframe**; `pdf` renders in a **plain iframe** so the browser's built-in PDF
viewer works (a sandbox blocks it) — safe because the PDF is cross-origin/passive and the
sender is trusted; `html`/`script` are same-origin and trusted (safe because pushes reach a
surface only over this local-trust socket, or an HTTP path gated by the surface's attach
token — DESIGN §8).

| `type` | fields | renders as |
|---|---|---|
| `navigate` | `{url}` | a sandboxed iframe to a third-party URL |
| `pdf` | `{url, page?}` | the browser's PDF viewer; `page` (1-based) deep-links into the document (`#page=N`) |
| `text` | `{body}` | a plain-text note |
| `link` | `{url, title?}` | a clickable link |
| `html` | `{markup}` | arbitrary same-origin DOM |
| `script` | `{code}` | arbitrary JS run in the surface page |

`page` is optional; omit it to open at the first page. Example — jump a long manual to page
348: `{"type":"pdf","url":"https://host/manual.pdf","page":348}`.

---

## 5. Errors

JSON-RPC `error` with a machine code in `data.code` (mirrors CONTROL-PROTOCOL.md §6):

| `data.code` | when |
|---|---|
| `unsupported-protocol` | `hello` major version the daemon can't speak |
| `not-ready` | a non-`hello` method before a successful `hello` |
| `invalid-params` | a required param is missing or malformed (bad `item`, no `surface-id`) |
| `invalid-surface-id` | `surface-id` is not a path-safe id |
| `no-such-item` | `set-view` names an unknown surface or `item-id` |
| `method-not-found` | unknown method |
| `parse-error` | a malformed JSON line (the connection stays open) |

```jsonc
{"jsonrpc":"2.0","id":4,"error":{"code":-32001,"message":"unknown surface or item-id",
 "data":{"code":"no-such-item"}}}
```

---

## 6. Relationship to the HTTP attachment surface

`surfaced` serves each surface to browsers over HTTP/SSE at `/s/{id}` (plus `/s/{id}/events`,
`/s/{id}/items`, `/s/{id}/view`); see DESIGN §10.1. `surface-ctl` and the HTTP surface act on
the **same** store, so:

- The **data-plane push paths** are all `surfaced`'s: `POST /s/{id}/items`, the MCP
  `send_item` tool (§8), and (as a local convenience) `send-item` over this socket. Any
  of them appears immediately on every attached browser tab (SSE fan-out) and in
  `list-items` / `GET /s/{id}/items` alike — they share one store.
- An **attach token** set via `create-surface` (or rotated/cleared later via `set-token`) is
  enforced on the HTTP side: attaching to a protected surface requires the token
  (`…/s/{id}?token=<t>`, or an `X-Surface-Token` / `Authorization: Bearer` header). This
  socket, being local-trust, is never token-gated.

---

## 7. What surfaced implements today

- The framing (§1), `hello` with `capabilities:["surface","durable-inbox","attach-fanout"]`.
- `create-surface` (title + attach token), `send-item` (all item types, `promote`),
  `set-view`, `list-items`, `list-surfaces`, `clear-items`, `remove-item`, `delete-surface`, `set-token`.
- Durable per-surface inbox (on-disk NDJSON, replayed on restart + compacted), bounded in memory.
  A surface's config also survives restart via per-surface sidecars: main-view selection, attach
  token (a protected surface stays protected), and title.
- SSE fan-out of pushes/view-changes to attached tabs, plus a live attach count.
- An **embedded MCP server** (§8) — the agent-facing data-plane API.

Later: attach-lifecycle notifications (daemon → capmeshd, e.g. pushing the attach count).

---

## 8. The embedded MCP server (agent-facing data plane)

Because **content push is the data daemon's job** (not capmeshd's — see the split in the
intro), `surfaced` hosts its **own MCP server** so an agent drives surfaces **directly**,
without capmeshd in the path. It is served at **`/mcp`** on `surfaced`'s HTTP server
(Streamable HTTP: the agent POSTs a JSON-RPC message and gets a JSON response; a
notification gets `202`; `GET /mcp` is `405` — no server-initiated stream). It shares the
same `SurfaceStore`, so an agent's push lands on the live surface a browser is attached to.

Tools:

| tool | arguments | does |
|---|---|---|
| `list_surfaces` | — | list every registered surface (id, title, item-count, current-view) |
| `list_items` | `{surface-id}` | the display items on a surface |
| `send_item` | `{surface-id, item, promote?}` | post a display item (created if new); `promote` (default `true`) also shows it in the main view |
| `set_view` | `{surface-id, item-id?}` | focus which existing item the main view shows (omit/null to clear) — re-focus without pushing |
| `remove_item` | `{surface-id, item-id}` | prune one item from the inbox by id (the `id` from `list_items`) |
| `clear_surface` | `{surface-id}` | empty a surface's inbox (the surface itself stays) |
| `delete_surface` | `{surface-id}` | delete a surface entirely (inbox + on-disk log) |

`item` is the display-item shape (§4): `{"type":"pdf","url":…}`, `{"type":"text","body":…}`,
`{"type":"link","url":…,"title":…}`, `{"type":"navigate","url":…}`, `{"type":"html","markup":…}`,
or `{"type":"script","code":…}`.

**Auth.** Optional: run `surfaced --mcp-token <t>` (or `SURFACED_MCP_TOKEN`) and the agent
sends `Authorization: Bearer <t>`; omitted, `/mcp` is open (trust the LAN / a reverse proxy).
Point an MCP client at `http://<host>:8787/mcp` (or `…/surfaced/mcp` behind nginx).
