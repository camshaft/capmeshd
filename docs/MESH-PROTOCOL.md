# `capmesh-mesh` — the peer↔peer mesh control endpoint

**Status:** v0.1 (design; capmeshd, 2026-09-27) — the wire the descriptor-fetch build targets.
**Companion:** [`DESIGN.md`](DESIGN.md) §5 (discovery + the `descr` pointer), §7 (the MCP surface),
§8 (the trust boundary); [`CONTROL-PROTOCOL.md`](CONTROL-PROTOCOL.md) (the *unrelated* local
`capmesh-ctl` seam — capmeshd ↔ its own data-plane daemons).

This is the **mesh control endpoint**: the read path one host's capmeshd exposes to *other* hosts'
capmeshd so a browser can turn a coarse `_capmesh._tcp` advert into a full typed descriptor. It is
the concrete form of the `descr` pointer promised in DESIGN §5.

It is **not** the data plane (media flows daemon↔daemon, peer-to-peer, and never touches this
endpoint) and **not** `capmesh-ctl` (that is the local Unix socket between capmeshd and a data-plane
daemon like `nmidid`). The mesh endpoint carries no media and no mutations — it answers descriptor
reads only.

## 1. Discovery → endpoint

A `_capmesh._tcp` record (DESIGN §5) carries the coarse, filterable keys plus two that locate this
endpoint:

- `ep=<port>` — the TCP port this endpoint listens on (the module's `advertisePort`, default 7420).
- `descr=/caps/<capability-id>` — the path at which the advertised capability's rich descriptor is
  served.

The browsing host **connects by the IP from the mDNS A/AAAA record** (DESIGN §5) — never by
resolving a `.local`/`.lan` name — to `<addr>:<ep>`.

## 2. Transport: HTTP/1.1 over TCP

The endpoint speaks **HTTP/1.1** over TCP. This is a declared default, chosen because the descriptor
fetch is a **read-only, cacheable, stateless GET** whose `descr=/caps/<id>` shape is already a URL
path; HTTP is the most interoperable substrate, needs no bespoke framing, and matches the transport
`surfaced` already serves. (The local `capmesh-ctl` seam stays NDJSON/JSON-RPC over a Unix socket —
a different problem: a stateful, mutating, same-host session. The two are deliberately distinct.)

All response bodies are UTF-8 JSON (`Content-Type: application/json`).

## 3. Methods (read-only)

### `GET /caps/<capability-id>` → a capability descriptor

Resolves the `descr` pointer. Returns the capability and its ports:

```json
{
  "id": "b1f0-uuid",
  "host": "green-machine",
  "kind": "midi",
  "dir": "source",
  "ports": [
    { "port-id": "kbd-0", "kind": "stream", "dir": "source", "type": "midi",
      "name": "Keystation 49e", "virtualizable": true,
      "formats": [ { "codec": "ump", "group": 0 }, { "codec": "midi1" } ] }
  ]
}
```

Each entry in `ports` is a **`PortDescriptor`** — the same shape `capmesh-ctl` `list-ports` returns
(`crates/capmesh-model`), reused verbatim so the model has one definition. This is what supplies the
remote `port-id`, the data-plane `port`, and the preference-ordered `formats` a mount needs — closing
the gaps that `connect`/auto-mount currently fill with hand-supplied values.

### `GET /caps` → all local capability descriptors

Returns `{ "caps": [ <capability descriptor>, … ] }` — every capability this host currently exposes,
for a browser that wants the whole surface in one round trip (enriches `discover`).

### Errors

Standard HTTP status: **404** for an unknown capability id, **503** while the underlying data-plane
daemon is unreachable (the descriptor cannot be built), **400** for a malformed path. A JSON body
`{ "error": "<machine-code>", "detail": "<human text>" }` accompanies a 4xx/5xx where useful.

## 4. Data source — a projection, not a store

capmeshd holds no authoritative descriptor state. It **builds** each response on demand from its
data-plane daemons over `capmesh-ctl` (`list-ports` / `describe-port`) and its configured advertised
kinds (DESIGN §4.1). The mesh endpoint is a **read-only projection** of what the host's daemons
report right now; there is nothing to invalidate.

## 5. Versioning

The advert's `v=1` (DESIGN §5) is the schema version. A future incompatible descriptor shape bumps
`v`; a browser MUST ignore an advert whose `v` it does not support (already enforced by
`capmesh-discovery`) and so never fetches an incompatible descriptor.

## 6. Trust

The endpoint is **read-only** and exposes only capability metadata (never media, never mutations),
so it sits inside the DESIGN §8 boundary as the least-sensitive surface. v0.1 relies on the trusted
LAN and the same-host `capmesh` group gating that protects the data plane; the path to a
cluster-credential header / mTLS underlay (§8) applies here unchanged and requires no protocol change
(a bearer/credential header is additive to the HTTP request). Discovery remains
convenient-but-untrusted: a descriptor fetched from an untrusted advert is **shown, never
auto-wired** (§8).
