# surfaced

A **browser-surface data-plane daemon**: durable, scriptable display sinks on the
mesh (DESIGN §10.1). A *surface* is a persistent, addressable inbox that a browser
attaches to; an agent (or any cluster-authenticated host) pushes display items to
it and they show up live on whatever device has the surface open.

- **A surface is a durable inbox, not a tab.** It lives in the daemon and survives
  restarts; a browser tab is just an ephemeral *attachment* to it. Close the tab,
  reopen it later, and the surface's items and selected view are still there.
- **Live.** Attachments stream updates over SSE, so a push (or a view change, or a
  removal) appears on every attached tab with no reload.
- **Rich but contained.** Items can be a third-party page, a PDF, text, a link, or
  trusted same-origin HTML/JS (see [item types](#item-types) and the trust model).

The control/MCP wire contract is specified in
[`docs/SURFACE-PROTOCOL.md`](../../docs/SURFACE-PROTOCOL.md); this README is the
overview and how-to-run.

## Run it

```sh
# ephemeral (surfaces are lost on restart)
cargo run -p surfaced -- --http-addr 127.0.0.1:8787

# durable, with the control socket + an MCP bearer token
cargo run -p surfaced -- \
    --http-addr 0.0.0.0:8787 \
    --state-dir /var/lib/surfaced \
    --socket /run/surfaced/ctl.sock \
    --mcp-token "$TOKEN"

# or from the flake
nix run .#surfaced -- --http-addr 0.0.0.0:8787 --state-dir /tmp/surfaces
```

Then open `http://<host>:8787/s/<surface-id>` (the id is created on first touch)
and push to it — see below.

### Flags

| flag | env | default | purpose |
|---|---|---|---|
| `-a, --http-addr` | | `127.0.0.1:8787` | HTTP/SSE bind address (pages + push path) |
| `-d, --state-dir` | | *(in-memory)* | directory for durable per-surface state; omit for non-durable |
| `-b, --base-path` | `SURFACED_BASE_PATH` | `""` | mount under a URL prefix (e.g. `/surfaced`) behind a reverse proxy |
| `-s, --socket` | `SURFACED_SOCKET` | *(off)* | Unix `surface-ctl` control socket capmeshd drives |
| `-m, --mcp-token` | `SURFACED_MCP_TOKEN` | *(open)* | bearer token required on the `/mcp` agent endpoint |
| `-l, --log-level` | | `info` | `trace`/`debug`/`info`/`warn`/`error` |

## HTTP endpoints

| method + path | purpose |
|---|---|
| `GET /` | health + build probe: `{service, status, version, assets:{js, css}}` — the `assets` hashes match the page's `?v=` fingerprints, so a deploy is verifiable |
| `GET /s/{id}` | the attachment page (main view + inbox drawer) |
| `GET /s/{id}/events` | SSE: an initial snapshot then live updates |
| `GET /s/{id}/items` | the surface's current state as JSON |
| `POST /s/{id}/items` | push a display item (`{item, promote?}`) |
| `DELETE /s/{id}/items/{item-id}` | remove one item |
| `POST /s/{id}/view` | select which item the main view shows (`{item-id?}`) |
| `POST /s/{id}/clear` | empty the inbox |
| `POST /mcp` | the embedded MCP server (agent-facing; see below) |

A push over HTTP and a `send-item` over the control socket are identical — both
append to the one store and fan out to every attached tab.

### Item types

`{navigate|pdf|text|link|html|script}`, internally tagged by `type`:

| type | fields | renders as |
|---|---|---|
| `navigate` | `{url}` | a third-party page in a sandboxed iframe |
| `pdf` | `{url, page?}` | the browser's PDF viewer; `page` (1-based) deep-links via `#page=N` |
| `text` | `{body}` | a plain-text note |
| `link` | `{url, title?}` | a clickable link |
| `html` | `{markup}` | trusted markup, isolated in an opaque-origin iframe |
| `script` | `{code}` | trusted JS, run in that isolated iframe |

`pdf` renders in a plain iframe (a sandbox blocks the native PDF viewer); it is
safe because the PDF is cross-origin/passive and the sender is trusted.

## Agent API (MCP)

`surfaced` embeds a Model Context Protocol server at `POST /mcp` (Streamable HTTP,
JSON-RPC 2.0) so an agent can drive surfaces directly. Tools: `list_surfaces`,
`list_items`, `send_item`, `set_view`, `remove_item`, `clear_surface`,
`delete_surface`. Optionally gate it with `--mcp-token`.

## Control socket (surface-ctl)

With `--socket`, `surfaced` also serves an NDJSON / JSON-RPC 2.0 Unix socket for
capmeshd's control plane (discovery + lifecycle): `hello`, `create-surface`,
`send-item`, `set-view`, `list-items`, `list-surfaces`, `clear-items`,
`remove-item`, `delete-surface`, `set-token`. Full shapes in
[`docs/SURFACE-PROTOCOL.md`](../../docs/SURFACE-PROTOCOL.md).

**Control vs. data plane.** capmeshd is control + service discovery only; all
content push / interaction is the data daemon's job (surfaced). The control-socket
`send-item`/`set-view` are a co-located convenience — capmeshd itself does not push
content.

## Durability & trust

- **Durable across restart:** each surface's items (an append-only, compacted
  NDJSON log) plus its main-view selection, attach token, and title (small
  sidecars) are replayed on startup.
- **Attach token (optional):** set per surface over the control socket
  (`create-surface`/`set-token`, never over HTTP). When set, HTTP attachment
  requires it (`?token=`, `X-Surface-Token`, or `Authorization: Bearer`).

## Deploy (NixOS)

The flake exposes `packages.surfaced` and `nixosModules.surfaced`
(`services.surfaced`: `address`/`port`/`basePath`/`socket`/`openFirewall`/
`stateDir`/`logLevel`), with `StateDirectory` durability and `DynamicUser`
hardening. Behind nginx, set `basePath` and proxy without stripping the prefix.
