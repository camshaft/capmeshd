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
| `send-item` | `{surface-id, item, promote?}` | `{id, ts}` | append a display item to the inbox (and, if `promote`, make it the main view) |
| `set-view` | `{surface-id, item-id?}` | `{}` | select which item the main view shows (`item-id` null/absent clears it) |
| `list-items` | `{surface-id}` | `SurfaceView` | the surface's current state (title, main view, items) |

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

The built-in item types (DESIGN §10.1). `navigate`/`pdf` are third-party URLs, rendered
inside a **sandboxed iframe** by the attachment page; `html`/`script` are same-origin and
trusted (safe because pushes reach a surface only over this local-trust socket, or an HTTP
path gated by the surface's attach token — DESIGN §8).

| `type` | fields | renders as |
|---|---|---|
| `navigate` | `{url}` | a sandboxed iframe to a third-party URL |
| `pdf` | `{url}` | a PDF viewer (sandboxed iframe) |
| `text` | `{body}` | a plain-text note |
| `link` | `{url, title?}` | a clickable link |
| `html` | `{markup}` | arbitrary same-origin DOM |
| `script` | `{code}` | arbitrary JS run in the surface page |

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

- A `send-item` over this socket appears immediately on every attached browser tab (SSE
  fan-out), and in `list-items` and `GET /s/{id}/items` alike.
- An **attach token** set via `create-surface` is enforced on the HTTP side: attaching to a
  protected surface requires the token (`…/s/{id}?token=<t>`, or an `X-Surface-Token` /
  `Authorization: Bearer` header). This socket, being local-trust, is never token-gated.

---

## 7. What surfaced implements today

- The framing (§1), `hello` with `capabilities:["surface","durable-inbox","attach-fanout"]`.
- `create-surface` (title + attach token), `send-item` (all item types, `promote`),
  `set-view`, `list-items`.
- Durable per-surface inbox (on-disk NDJSON, replayed on restart), bounded in memory.
- SSE fan-out of pushes/view-changes to attached tabs.

Later: attach-lifecycle notifications (daemon → capmeshd, e.g. attach-count), a
`delete-surface`/`clear` method, and log compaction. The MCP `send` tool (DESIGN §7) lives
on capmeshd's MCP server and drives `send-item` over this socket.
