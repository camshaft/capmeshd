# capmesh data-plane control protocol (`capmesh-ctl`, first daemon: `nmidid`)

**Status:** v0.1 draft (design-capmesh, 2026-09-27) — for the M0a nmidi vertical to implement.
**Companion:** [`DESIGN.md`](DESIGN.md) §4 (plugins drive data-plane daemons over control sockets),
§3.1 (typed ports), §3.2 (virtual endpoints), §6 (reconcile).

This is the **local seam** between capmeshd (an in-process adapter, §4) and a **data-plane daemon**
that owns a capability's data plane. `nmidid` (extended `nmidi`) is the first daemon; the protocol
is written to generalize to any future first-party daemon. Native third-party daemons
(PipeWire/Moonraker/wayvnc) are *not* required to speak it — their adapter speaks their own protocol
(§4). This spec governs only first-party data-plane daemons.

---

## 1. Transport & framing

- **Unix domain socket** (default `/run/<daemon>.sock`, e.g. `/run/nmidid.sock`). Local only —
  capmeshd and the daemon are always co-located on one host.
- **Framing:** newline-delimited JSON (**NDJSON**), one JSON value per line.
- **Semantics:** **JSON-RPC 2.0**. Requests carry an `id`; the daemon replies with a matching
  `result` or `error`. **Notifications** (no `id`) flow daemon→capmeshd for asynchronous events.
- **Direction:** capmeshd is the client (issues requests); the daemon is the server (replies +
  emits notifications). One socket connection per capmeshd↔daemon pair.

### 1.1 Trust

The control socket is **local trust only** — guarded by filesystem permissions (owner `capmesh`
user/group). It carries **no cluster key**: the cluster credential (DESIGN §8) authenticates the
*inter-host* mesh channel, not this local socket. A daemon MUST refuse connections whose socket
peer credentials are outside the configured owner/group.

### 1.2 Handshake & versioning

The first request on a connection MUST be `hello`. The daemon rejects all other methods until a
compatible `hello` completes.

```jsonc
// → request
{"jsonrpc":"2.0","id":1,"method":"hello",
 "params":{"protocol":"1","client":"capmeshd/0.1"}}
// ← result
{"jsonrpc":"2.0","id":1,"result":{
   "protocol":"1", "daemon":"nmidid/0.1",
   "capabilities":["virtual-endpoints","hotplug-events","midi1","ump"]}}
```

`protocol` is a single integer major version; a daemon MUST reject an unknown major with error
`unsupported-protocol`. `capabilities` advertises optional features the daemon supports (see §5).

---

## 2. The typed-port model on the wire

Every capability the daemon owns is a **node** exposing **ports** (DESIGN §3.1). A `PortDescriptor`:

```jsonc
{
  "port-id": "kbd-0",              // stable within this daemon/host
  "kind": "stream",               // "stream" | "rpc"
  "dir": "source",                // stream ports only: "source" | "sink"
  "type": "midi",                 // capability type (midi | audio | video | ...)
  "name": "Keystation 49e",
  "virtualizable": true,          // daemon can materialize a local virtual mirror (DESIGN §3.2)
  "formats": [                    // PREFERENCE-ORDERED list of supported formats (§4)
    {"codec":"ump"},
    {"codec":"midi1"}
  ]
}
```

A `Format` is `{"codec": "<name>", ...codec-specific params}`. For MIDI:

| codec    | meaning                              | params |
|----------|--------------------------------------|--------|
| `midi1`  | classic MIDI 1.0 byte stream         | — |
| `ump`    | Universal MIDI Packet (MIDI 2.0)     | `{"group": <0-15>?}` |

Audio/video (forward-looking, other daemons) use the *same* `Format` shape, e.g.
`{"codec":"pcm","rate":48000,"channels":2,"sample":"f32le"}`, `{"codec":"opus","rate":48000}`,
`{"codec":"h264","width":1920,"height":1080,"fps":60}` — see §4.3.

---

## 3. Methods (capmeshd → daemon)

| method | params | result | purpose |
|---|---|---|---|
| `hello` | `{protocol, client}` | `{protocol, daemon, capabilities}` | handshake (§1.2) |
| `list-ports` | `{}` | `{ports: [PortDescriptor]}` | enumerate current local ports (feeds advertise + auto-mount) |
| `describe-port` | `{port-id}` | `PortDescriptor` | full descriptor for one port |
| `mount` | `MountSpec` (§3.1) | `{mount-id, state}` | establish a mount; idempotent on `mount-id` |
| `unmount` | `{mount-id}` | `{}` | tear a mount down; idempotent (unknown id = ok) |
| `mount-status` | `{mount-id?}` | `{mounts: [MountStatus]}` | one or all live mounts |

### 3.1 `MountSpec` — the mount request (carries the negotiated format)

```jsonc
{
  "mount-id": "b1f0…",           // capmeshd-assigned UUID; the reconciler's idempotency key
  "role": "mirror-source",       // §3.2
  "local":  {                     // the local endpoint the daemon owns/creates
     "virtual": true,             // create a virtual endpoint (requires port.virtualizable)
     "name": "laptop: Keystation 49e"   // display name for the virtual device
  },
  "remote": {                     // the peer this host connects to (DIRECT, p2p)
     "host": "laptop",
     "addr": "192.168.1.23",      // ALWAYS the IP from the mDNS record, never a .local/.lan name
     "port": 5004,
     "port-id": "kbd-0"
  },
  "format": {"codec":"midi1"}     // the CHOSEN format — result of negotiation (§4), not a list
}
```

**`role`** (which end the daemon materializes):

| role | meaning |
|---|---|
| `mirror-source` | create a local **virtual source** fed by the remote source (the keyboard-shows-up case) |
| `mirror-sink`   | create a local **virtual sink** that forwards to the remote sink |
| `link`          | connect an existing local **real** port to the remote (no virtual endpoint) |

### 3.2 `MountStatus`

```jsonc
{"mount-id":"b1f0…", "state":"active",
 "since":"2026-09-27T18:04:11Z",
 "stats":{"bytes-in":10432,"bytes-out":0,"last-event":"2026-09-27T18:07:52Z"},
 "detail":null}
```

`state` ∈ `pending | connecting | active | degraded | failed | torn-down`. The reconciler
(DESIGN §6) drives self-heal off these: on `failed` it retries per policy; `degraded` is live but
impaired (e.g. clock-sync loss) and surfaced in `status`/MCP.

---

## 4. Format negotiation

Negotiation is **transport-agnostic** and lives mostly in **capmeshd** (the daemon only confirms it
can honor the chosen format). This keeps `nmidid` simple while making the audio case (where it
matters most) work under the same rules.

### 4.1 The algorithm (capmeshd side, at `connect`)

1. Fetch the **remote source** descriptor (over the mesh descriptor endpoint, DESIGN §5) and the
   **local sink** descriptor (via `list-ports`/`describe-port`).
2. Compute the **compatible set**: formats that appear on both sides, matched by `codec` **and**
   compatible params (§4.2). Preserve the **consuming side's preference order** as the ranking.
3. **Pick the top-ranked compatible format** → this becomes `MountSpec.format`.
4. **Empty compatible set → `connect` is refused** with `no-common-format`, and the error `data`
   lists both sides' `formats` so the agent/operator can see why.
5. Send `mount` with the chosen format. The daemon MAY still reject at runtime with
   `format-unsupported` (a real-world constraint the descriptor didn't capture); capmeshd surfaces
   that verbatim.

### 4.2 Param compatibility & conversion

- **Exact-match params** (e.g. MIDI `codec`) → compatible iff equal.
- **Convertible params** (audio `rate`, `channels`; MIDI `midi1`↔`ump`) → compatible iff a
  **converter** is available. A converter is declared as a daemon `capability` (§1.2) or provided by
  the transport itself (roc/PipeWire resample + up/down-mix). When a mount relies on a converter,
  capmeshd sets `format.convert: true` and names the on-wire format; the receiver inserts the
  conversion. Absent any converter, differing convertible params are **incompatible** (fall to §4.1
  step 4).
- Preference: capmeshd prefers a **direct (no-convert)** common format over a converted one, then
  ranks by the consuming side's order.

### 4.3 Per-type notes

- **MIDI (M0):** the common case is trivial — both sides do `midi1`; negotiation almost always
  picks `midi1`. `midi1↔ump` is an *optional* converter (`capabilities:["midi1","ump"]`); if a
  daemon lacks it, a `ump`-only source and a `midi1`-only sink are `no-common-format`.
- **Audio (later daemon):** `rate` and `sample`/`channels` are the load-bearing negotiation. roc /
  PipeWire supply clock-domain resampling and mixing, so most rate/channel mismatches resolve via
  `convert:true`; `codec` (pcm vs opus) must match or be converted by an explicit transcoder.
- **Video (later):** `codec`+resolution+fps; typically negotiated down to a common codec, no
  transcode in v1 (refuse if none common).

---

## 5. Notifications (daemon → capmeshd)

Unsolicited, no `id`. capmeshd re-advertises / re-reconciles on these — this is what powers
**auto-mount** (DESIGN §6.1): a hot-plugged device emits `port-added`, capmeshd matches it against
auto-mount selectors and issues a `mount`.

| method | params | meaning |
|---|---|---|
| `port-added` | `{port: PortDescriptor}` | a device appeared (hot-plug) — gated behind `hotplug-events` capability |
| `port-removed` | `{port-id}` | a device went away → capmeshd tears down dependent mounts |
| `mount-state` | `{mount-id, state, detail?, stats?}` | a mount changed state (§3.2) |

```jsonc
{"jsonrpc":"2.0","method":"port-added",
 "params":{"port":{"port-id":"kbd-0","kind":"stream","dir":"source","type":"midi",
                   "name":"Keystation 49e","virtualizable":true,
                   "formats":[{"codec":"ump"},{"codec":"midi1"}]}}}
```

A daemon without `hotplug-events` MUST support periodic `list-ports` polling instead; capmeshd falls
back to polling when the capability is absent.

---

## 6. Errors

JSON-RPC `error` with a machine code in `data.code`:

| `data.code` | when |
|---|---|
| `unsupported-protocol` | `hello` major version the daemon can't speak |
| `not-ready` | a non-`hello` method before a successful `hello` |
| `no-such-port` | `describe-port`/`mount` names an unknown local port |
| `no-common-format` | negotiation found no compatible format (`data.local`, `data.remote` list both) |
| `format-unsupported` | daemon cannot honor the chosen `format` at runtime |
| `virtual-unsupported` | `local.virtual:true` on a non-`virtualizable` port |
| `peer-unreachable` | the daemon could not reach `remote.addr:port` |
| `busy` | the port/device is exclusively held, or a local virtual endpoint could not be created |
| `role-unsupported` | `mount` names a `role` (§3.1) the daemon does not implement |
| `internal` | an unexpected daemon-side failure; `message` carries the detail |

```jsonc
{"jsonrpc":"2.0","id":7,"error":{"code":-32001,"message":"no common format",
 "data":{"code":"no-common-format",
         "local":[{"codec":"midi1"}],"remote":[{"codec":"ump"}]}}}
```

---

## 7. What the M0a `nmidid` vertical implements

- The socket + NDJSON/JSON-RPC framing (§1), `hello` with `capabilities:["virtual-endpoints",
  "hotplug-events","midi1"]` (`ump` optional).
- `list-ports`/`describe-port` from the existing `midir` port scan (`nmidi-server/src/midi.rs`).
- `mount` for `role:"mirror-source"` (the keyboard case): create a virtual ALSA/CoreMIDI input via
  `midir::create_virtual`, run the AppleMIDI `IN/OK/CK` handshake to `remote`, pump RTP-MIDI into the
  virtual port. `unmount` drops it. (This is nmidi's two current `TODO`s, behind the socket.)
- `port-added`/`port-removed` from the existing port monitor (`start_port_monitor`), `mount-state`
  transitions.
- MIDI negotiation is trivial (§4.3) — accept the `format` capmeshd chose; reject unknown codec with
  `format-unsupported`.

Everything else (audio/video codecs, converters) is later-daemon scope; the wire shape is fixed here
so those daemons drop in without a protocol change.
