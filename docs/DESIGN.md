# capmeshd — LAN capability mesh

**Status:** v0.1 design draft (design-capmesh fleet agent × operator, 2026-09-27)
**Anchor code:** [`camshaft/nmidi`](https://github.com/camshaft/nmidi) (MIDI-over-network seed),
[`camshaft/dotfiles`](https://github.com/camshaft/dotfiles) (NixOS fleet, Colmena, Avahi),
[`camshaft/printers`](https://github.com/camshaft/printers) (Voron / Klipper / Moonraker).
**Companion research:** `~/Projects/lan-capability-mesh-design.md` (landscape survey; build-vs-compose verdict).

---

## 1. What it is

A **per-host Rust daemon (`capmeshd`)** that advertises the machine's typed **capabilities** over
the LAN, and — on request from an AI agent or a declarative config — **mounts** a remote
capability locally by driving the *existing* transport for that capability kind. It is a thin,
typed, agent-driven **control plane** over transports that already exist (RTP-MIDI, PipeWire/roc,
Sunshine/wayvnc, Moonraker/PrusaLink). It does **not** reimplement transports, codecs, sync, or
deployment.

The mental model is a **LAN-native, distributed IFTTT**: pipe anything to anything, on the fly,
local-first, controllable by agents.

> **The one-line architecture:** `capmeshd` is **stateless control-plane plumbing** — it advertises
> typed capabilities over DNS-SD and reconciles declarative "mounts" by driving each capability's
> own **data-plane daemon over a control socket** (never carrying data or transport state itself),
> with an **MCP server** (that any host can run as a mesh client, not a hub) exposing
> discover / describe / connect / disconnect / status to agents, and **NixOS modules** that make
> advertisement and permanent mounts a single declarative act deployed with Colmena.

### The build-vs-compose split (why this is small)

- **~90% compose** — PipeWire + WirePlumber (local audio/video graph), roc + PipeWire-RTP/AES67
  (audio legs), **nmidi** (MIDI leg), Sunshine/wayvnc (screen leg), Moonraker/PrusaLink (printer
  control), Avahi / `mdns-sd` (discovery), Colmena (deploy). All already open, all already in the
  operator's stack (`services.avahi.enable = true` is already set in `dotfiles/roles/networking.nix`).
- **~10% build** — the four novel pieces: (1) a typed **capability descriptor** + a DNS-SD
  advertisement convention that carries it, (2) the **daemon** that reconciles mounts by driving
  transports, (3) the **MCP server** over the control API, (4) the **NixOS modules**.

---

## 2. Architecture — control plane vs data plane

The load-bearing decision: **centralize only the *view*, never the data.**

```
                       ┌──────────────────────── AI agent ─────────────────────────┐
                       │        MCP: discover / describe / connect /                 │
                       │                disconnect / status / invoke                 │
                       └──────────────────────────┬──────────────────────────────────┘
                                Streamable HTTP / stdio  (ONE endpoint)
                                                   │
                                    ┌──────────────▼───────────────┐
                                    │  MCP server = a MESH CLIENT   │   any host may run it;
                                    │  (browses mDNS, fans control  │   NOT a routing hub;
                                    │   RPCs out to peer daemons)    │   no data flows through it
                                    └───────┬───────────────┬────────┘
                       control RPC          │               │  control RPC
   host A (source)     (cluster-key auth)   │               │                 host B (sink)
   ┌──────────────────────────┐   DNS-SD    ▼               ▼   DNS-SD    ┌──────────────────────────┐
   │ capmeshd (stateless)     │◄───────────  _capmesh._tcp  ───────────►│ capmeshd (stateless)     │
   │  • advertise caps        │  (distributed discovery; zero-config)   │  • reconcile actual↔desired│
   └───────────┬──────────────┘                                        └────────────┬──────────────┘
    control socket │ (drive LOCAL data-plane daemon; capmeshd carries no data)  control socket │
                   ▼                                                                  ▼
   ┌──────────────────────────┐      DATA PLANE — daemon↔daemon,        ┌──────────────────────────┐
   │ data-plane daemon        │      DIRECT peer-to-peer                │ data-plane daemon        │
   │  nmidid / PipeWire /     │◄══════════════════════════════════════►│  nmidid / PipeWire /     │
   │  Moonraker / Sunshine    │  RTP-MIDI / roc·AES67 / WS / video      │  Moonraker / Sunshine    │
   └───────────┬──────────────┘                                        └────────────┬──────────────┘
               ▼                                                                     ▼
   physical MIDI device / PipeWire node / screen / printer            local virtual endpoint
```

- **Discovery = distributed (mDNS/DNS-SD).** No host registers to a hub. Daemons announce and
  browse `_capmesh._tcp`. Join/leave is zero-config — the plug-n-play property the operator wants.
- **`capmeshd` = stateless control-plane plumbing.** It holds no persistent authoritative state and
  **never carries data**. It reconciles mounts by driving each capability's own **data-plane daemon
  over a control socket** (§4); a restart rebuilds actual-state from declarative config + querying
  those sockets.
- **Data plane = strictly peer-to-peer, direct — daemon↔daemon.** A mount is a direct connection
  between the two hosts' data-plane daemons (nmidid↔nmidid, etc.). **No media ever transits a hub,
  and it never transits capmeshd** → no bottleneck, no single point of failure for live wiring.
- **Control plane / MCP = a mesh *client*, not a hub.** The MCP server browses the mesh via mDNS
  and issues control RPCs to peer capmeshd daemons. The agent gets **one endpoint**; the distributed
  model is preserved; if the MCP host dies, every existing mount keeps running because each sink
  daemon self-heals its own desired-state.

---

## 3. The capability & mount model

A **capability** is a typed, addressable thing a host offers or consumes. Borrows openHAB's
Channel/Link and Matter's cluster/attribute *shapes* without their governance baggage.

- **Kind** — `midi | audio | screen | control-api | generic-stream | generic-rpc`.
- **Direction** — `source` (produces) | `sink` (consumes) | `duplex` | `control` (request/response).
- **Type descriptor** — a small **versioned** schema per kind, e.g.
  - `midi`  → `{ ports: [{ name, dir, index }] }`
  - `audio` → `{ channels, rate, formats: [pcm, opus], transport: [roc, rtp, aes67] }`
  - `screen`→ `{ resolution, codecs: [h264, hevc], transport: [sunshine, rfb], role: [mirror, display-url] }`
  - `control-api` → `{ protocol: "moonraker-jsonrpc" | "prusalink-http", endpoint, methods }`
- **Identity** — stable `capability-id` (UUID) + `host-id` + transport coordinates.
- **Transport binding** — which transport realizes it, and its parameters.
- **Lifecycle** — `advertised → offered → mounted(temp|permanent) → torn-down`; temp carries a TTL.

A **mount** is a directed edge *(source cap on host A) → (sink cap on host B)* plus lifetime
(temp+TTL or permanent) and transport params. The set of desired mounts is **declarative state**
the daemons reconcile. This is what makes it IFTTT-like and lets permanent mounts survive reboots.

### 3.1 Unified typed ports (source / sink / rpc)

The data-plane concepts **are unified** — borrowing PipeWire's proven node/port/link shape,
mesh-wide. A capability is a **node** exposing typed **ports**. Each port has:

- a **port kind**: **`stream`** (a typed byte/event stream — midi/audio/video), **`rpc`** (a
  request/response control handle — e.g. Moonraker), or **`surface`** (a durable, scriptable display
  sink — §10.1); and
- for `stream` ports, a **direction**: **`source`** (produces) or **`sink`** (consumes).

A **mount is a type-compatible link** between ports: a `stream` source links only to a `stream`
sink of a compatible type (MIDI↔MIDI, an audio format both accept, …); type mismatch is refused at
`connect`. This one model spans every streaming kind and lets the agent/auto-mount reason
generically ("wire every `midi` source to …") without per-kind special cases.

**The honest caveat — not everything is a stream.** A **control-API is request/response, not a
source/sink**. So `control-api` capabilities expose an **`rpc` port**: mounting one materializes a
proxied *handle* (the sink can `invoke` methods), rather than establishing a streaming link. `rpc`
lives under the same node/port umbrella so discovery, mounts, and the MCP surface stay uniform —
Moonraker is not forced to pretend to be a stream.

### 3.2 Virtual endpoints (why a remote device "just shows up" locally)

A transport that can **materialize a local virtual endpoint** declares that capability in its
descriptor (`virtualizable: true`). Then a mount doesn't just move bytes — it makes the remote port
**appear as a native local device**:

- Mounting a remote **`stream` source** → the local data-plane daemon creates a **local virtual
  source** that native apps read from as if the device were plugged in here (the operator's
  virtual-MIDI-input case: a remote keyboard shows up as a local MIDI input).
- Mounting a remote **`stream` sink** → a **local virtual sink** apps write to.

The daemon (e.g. `nmidid` via `midir::create_virtual`) bridges the virtual endpoint ↔ the remote
real device over the transport. capmeshd only *asks* for the virtual endpoint over the control
socket; the daemon owns creating and pumping it.

---

## 4. The plugin system — capmeshd drives data-plane daemons over control sockets

**`capmeshd` is stateless plumbing.** It owns discovery, the desired-mount reconciler, the MCP
surface, and cross-host coordination — but it **never links a transport crate and never carries
data**. Each capability's *data plane* is owned by its **own daemon**, which exposes a **control
socket**; a capmeshd **plugin is a thin client of that control socket**. This is the operator's
directive: keep capmeshd stateless plumbing, push the data plane out to per-capability daemons.

**The plugins live *inside* capmeshd — there is no separate per-capability "mapper" daemon.** Each
plugin is an **in-process adapter module** compiled into capmeshd that knows how to speak one
data-plane protocol; a **TOML config** (§4.1) declares which protocols this host supports and how to
reach each data-plane daemon (socket path / endpoint / params). The only processes on a host are
capmeshd plus the *real* data-plane services that would run regardless (PipeWire, Moonraker,
`nmidid` for MIDI) — capmeshd never spawns a translation daemon of its own. "capmeshd links no
transport crate" still holds: an adapter speaks a **protocol** (Unix socket / WebSocket / CLI), it
does not link the transport's stack.

```rust
/// One implementation per capability kind. Each plugin is a CLIENT of a local
/// data-plane daemon's control socket — it holds no transport state itself.
trait CapabilityPlugin {
    fn kind(&self) -> CapabilityKind;

    /// Ask the local data-plane daemon what this host offers/consumes for this
    /// kind (e.g. nmidid's port list). Feeds the advertiser + descriptor endpoint.
    async fn scan_local(&self) -> Vec<Capability>;

    /// Full typed descriptor for a locally-advertised capability.
    async fn describe(&self, cap: &CapabilityId) -> Descriptor;

    /// Instruct the LOCAL data-plane daemon (over its control socket) to
    /// establish/attach a peer-to-peer data connection to the remote source
    /// (or accept from the remote sink). capmeshd is not in the data path.
    /// The returned MountHandle's Drop tells the daemon to tear the link down.
    async fn mount(&self, spec: &MountSpec) -> Result<MountHandle>;

    /// Query the data-plane daemon for liveness / throughput / last-seen.
    async fn status(&self, mount: &MountId) -> MountStatus;
}
```

**Two flavors of control socket, both behind the same trait — this is the key generality:**

- **Daemons with a native control protocol** (audio/screen/printer): the plugin speaks whatever the
  service already exposes — as rich as a WebSocket JSON-RPC or **as thin as a CLI invocation** —
  `pw-cli`/libpipewire for PipeWire, `wayvncctl` JSON-IPC or Sunshine for screen, Moonraker WebSocket
  JSON-RPC / PrusaLink HTTP for the printer. No new daemon to write; the mesh hides the differences.
- **Daemons we extend to speak a *capmesh control protocol*** (MIDI): **nmidi is extended into a
  data-plane daemon (`nmidid`) that exposes a control socket** — "create a virtual ALSA/CoreMIDI
  port and connect RTP-MIDI to `<peer addr>`" / "tear it down" — and owns the AppleMIDI handshake +
  the bidirectional pump entirely. capmeshd's MIDI plugin just drives that socket. **This is a
  separate nmidi workstream** (see §11): closing nmidi's two `TODO`s *and* wrapping them behind a
  control socket, so nmidi becomes "purely in charge of the MIDI data plane."

The capmesh control protocol (flavor 2) is a small JSON-RPC 2.0 / NDJSON protocol over a Unix domain
socket, specified in [`docs/CONTROL-PROTOCOL.md`](CONTROL-PROTOCOL.md): `hello`, `list-ports`,
`describe-port`, `mount`, `unmount`, `mount-status`, plus hot-plug/state notifications and
format negotiation. Any future first-party data-plane daemon
implements it and drops straight into capmeshd. Adding a capability kind = one plugin (a socket
client) + either an existing daemon's native protocol or a small daemon speaking this protocol.
`MountHandle: Drop` gives idempotent teardown (the reconciler converges "unmounted" by dropping it).

### 4.1 The data-plane config (TOML)

capmeshd is **config-driven**: a TOML file declares which data-plane protocols this host supports
and how each in-process adapter reaches its local data-plane daemon. The NixOS module (§9) renders
this file from `services.capmesh.advertise.*`, so the operator never hand-writes it — but it is the
single, inspectable source of "what this host can plumb."

```toml
# /etc/capmesh/capmesh.toml  (rendered by the NixOS module)
host-id = "green-machine"
cluster-key-file = "/run/agenix/capmesh-cluster.key"

[dataplane.midi]
protocol = "nmidi-ctl"                       # the capmesh control-socket protocol (§4, flavor 2)
socket   = "/run/nmidid.sock"                # nmidid owns the RTP-MIDI data plane

[dataplane.audio]
protocol = "pipewire"                        # native protocol; adapter drives pw-cli / libpipewire

[dataplane.printer.voron]
protocol = "moonraker-jsonrpc"               # native protocol
endpoint = "ws://127.0.0.1:7125/websocket"

[dataplane.printer.prusa-mk4]
protocol = "prusalink-http"
endpoint = "http://prusa-mk4.lan/api"
```

A plugin adapter is selected by `protocol`; adding a host binding is a config line, adding a *new*
protocol is one in-process adapter module. No adapter → no advertisement for that kind on that host.

---

## 5. Discovery layer

- **Substrate: DNS-SD/mDNS** via Avahi (already enabled) + the `mdns-sd` crate (already used by
  nmidi). One service type `_capmesh._tcp.local` = the daemon's control endpoint.
- **TXT carries only coarse, filterable keys — not the schema:**
  `cap=midi`, `dir=source`, `id=<uuid>`, `host=<host-id>`, `v=1`, `ep=<port>`,
  `descr=/caps/<uuid>` (a pointer to fetch the rich descriptor over the daemon's own HTTP/RPC).
  This is the standard DNS-SD idiom and sidesteps TXT's size/typing limits.
- **⚠ Connect by IP from the mDNS A/AAAA record, never by resolving `<peer>.local`.** (The operator
  has moved off `.local` to `.lan` — but resolving discovered peers by the address in the service
  event is the robust design regardless of split-horizon DNS config.)
- **Generalize, don't rewrite:** nmidi's `ServiceAdvertiser` / `browse_services`
  (`nmidi-core/src/discovery.rs`) is *already* the generic discovery primitive — it just needs a
  `_capmesh._tcp` type and the `descr` pointer. Lift it into a `capmesh-discovery` crate.
- **Interop ingest:** keep native service types where they buy interop (`_apple-midi._udp` so
  macOS/iOS/rtpMIDI see MIDI; `_octoprint._tcp`; Cast) and *also* ingest them to surface
  third-party capabilities — as **untrusted, read-only** (see §8).

---

## 6. Control plane & reconciliation

- **Reconcile, not imperative RPC.** Each `capmeshd` holds a **desired-mount set** — permanent
  mounts from the local NixOS config, temp mounts from live agent/MCP commands — and continuously
  reconciles actual vs desired. Idempotent, self-healing after a peer reboot, clean
  permanent-vs-temp semantics (temp = TTL lease; miss the renewal → torn down).
- `connect` / `disconnect` are just **mutations of desired-state**, not direct imperative calls.
- **Connect flow (sink-initiated):** agent calls `connect(source-cap, sink-host)` → sink's
  `capmeshd` looks the source up via discovery, fetches its descriptor, negotiates a transport both
  support, invokes the kind's plugin `mount(...)`, and records the mount in desired-state.

### 6.1 Auto-mount subscriptions (plug-n-play, zero commands)

A host may declare **auto-mount selectors** — the reconciler *derives* desired mounts from live
discovery instead of only from explicit `connect` calls. This is the plug-n-play magic:

```toml
# any MIDI source that appears on the mesh → mirror it here as a local virtual MIDI input
[[automount]]
match  = { kind = "midi", port = "stream", dir = "source" }   # selector over the typed model (§3.1)
action = "mirror-local"                                        # materialize a local virtual endpoint (§3.2)
lifetime = "while-advertised"                                  # torn down when the advert vanishes
```

The reconciler watches mDNS `_capmesh._tcp` events; a new advertisement matching a selector is
**automatically added to desired-state** (and removed when it disappears). So: plug a MIDI keyboard
into the laptop → the laptop advertises a `midi` `stream`/`source` capability → the desktop's
selector fires → `nmidid` materializes a local virtual MIDI input fed by the keyboard, with no
command issued. Unplug → the advertisement drops → the mount tears down. Selectors are per-host,
opt-in, and gated by the §8 trust boundary (only cluster-authenticated advertisements auto-mount;
a stray/third-party advert is never auto-wired).

---

## 7. The MCP surface

Thin MCP server (Rust; `rmcp` / official SDK), **Streamable HTTP** transport (long-running daemon,
matches Home Assistant's proven pattern) + stdio for local dev. Runs as a mesh client (§2).

capmeshd's MCP surface is **control plane + service discovery only** — it wires and inspects mounts;
it never carries or pushes data. Moving data (pushing a display item to a surface, streaming MIDI)
is the **data-plane daemon's** job, and each data daemon exposes its own data verbs on its own MCP.
So the surface item-push verb lives on `surfaced`'s embedded MCP (`send_item`, alongside
`list_surfaces` / `list_items` — see §10.1), **not** here.

- **Tools** (all control-plane)
  - `discover(kind?, dir?, host?)` → capabilities (id, kind, dir, host, summary).
  - `describe(capability-id)` → full typed descriptor + current mounts.
  - `connect(source-id, sink-id, lifetime=temp|permanent, ttl?, transport?)` → mount-id.
  - `disconnect(mount-id)`.
  - `status(mount-id? | host? | all)` → live health/throughput/last-seen.
  - `invoke(capability-id, method, params)` → escape hatch for `control-api` kinds (Moonraker
    `printer.*`, PrusaLink endpoints).
- **Resources** — `capmesh://topology` (current graph snapshot), `capmesh://host/<id>` (read-only
  live state).
- **Prompts** — worked examples ("connect the studio keyboard to the SuperCollider host temporarily").

This is precisely the "mDNS + generic connect/disconnect MCP server" the landscape survey found
nobody has built.

---

## 8. Security — the dotfiles trust boundary

The trust boundary is **exactly the set of hosts managed by `dotfiles`** — clean and closed.

- A **cluster credential** (start: a pre-shared cluster key / token; path: per-host mesh certs / a
  small mesh CA) is **provisioned via `dotfiles`' existing `age` secrets** (the fleet already does
  host-key-decrypted age secrets — e.g. `dotfiles/roles/3d.nix`'s `age.secrets."prusa-mk4.conf"`).
- **Every *actionable* RPC** (control channel + mount/data-plane setup) requires the cluster
  credential. A device that isn't dotfiles-managed **cannot join, cannot be mounted, cannot mount**.
- **mDNS discovery is convenient-but-untrusted.** Third-party advertisements (`_octoprint._tcp`,
  Cast, a stray daemon) may be *seen* in `discover` but are **never auto-wired** — wiring an
  untrusted source requires an explicit operator opt-in.
- **The local control socket is gated by a shared group.** A data-plane daemon reads the connecting
  peer's OS credentials (`SO_PEERCRED`) and admits only its own uid plus members of a shared, stable
  `capmesh` group; capmeshd's process (kept under `DynamicUser`) joins that group via
  `SupplementaryGroups`. Because `capmesh` is a *supplementary* group on the DynamicUser (whose
  primary gid is the transient dynamic one), the daemon matches the peer's **full** group set, not
  just its primary gid. This keeps a co-resident non-capmesh process off the control socket even
  behind the `0o660` socket perms. The group is authorized **by name** — an auto-allocated group's
  gid is not known at module-eval time, so the deployment enables enforcement by naming the shared
  group on the daemon (§9), never by pinning a numeric gid. Enforcement is opt-in per host and
  defaults off (non-breaking).
- **Path to mTLS/WireGuard** underlay later with no protocol change (the descriptor-fetch and
  control channel are designed to carry it).

---

## 9. NixOS integration (single flake, Colmena)

Deployment + advertisement + permanent mounts as **one declarative act**, deployed with **Colmena**
(the operator's existing tool; `deployment.tags` already in use — tags double as a capability axis).
The Voron/`printers` and MIDI/audio config all converge into the single `dotfiles` flake.

```nix
services.capmesh = {
  enable  = true;
  hostId  = "green-machine";
  advertise = {
    midi.enable    = true;                       # runs the MIDI plugin; publishes _apple-midi + _capmesh
    audio.enable   = true;
    klipper = { enable = true; moonraker = "ws://127.0.0.1:7125/websocket"; };
  };
  mayMount        = [ "midi" "audio" ];          # kinds this host is allowed to consume
  permanentMounts = [                            # reconciled every boot (self-heal)
    { source = "studio/midi/keyboard"; sink = "music-host/midi/in"; }
  ];
  clusterKeyFile = config.age.secrets."capmesh-cluster.key".path;   # trust boundary
};
```

The module runs `capmeshd`, relies on the already-enabled Avahi (and can publish the
`_moonraker._tcp` record Moonraker omits), opens the right firewall ports, and installs
`permanentMounts` into desired-state. `colmena apply --on @printer` pushes to every tagged host.

**Enabling the local-socket trust boundary (§8).** `services.capmesh` places capmeshd's process in
a shared `capmesh` group (`SupplementaryGroups`, keeping the `DynamicUser` sandbox) and declares the
group. To turn peer-credential enforcement on for a data-plane daemon, authorize that group **by
name** — for `nmidid`:

```nix
services.nmidid.allowedGroups = [ config.services.capmesh.group ];   # default "capmesh" on both sides
```

The daemon resolves each name to a gid from `/etc/group` at startup (an unresolved name is a fatal,
fail-closed start error) and matches the connecting peer's **full** group set, so the DynamicUser
capmeshd client is admitted while a co-resident non-`capmesh` process is refused — with no numeric
gid pinned anywhere. Enforcement defaults **off** (empty `allowedGroups`), so it is opt-in per host.

Hosts today: Linux `gateway` / `green-machine` / `i7-machine`; macOS `camerons-mini` /
`camerons-work-mbp`. **The mesh is cross-OS from day one** — the MIDI plugin creates virtual ports
on both ALSA (Linux) and CoreMIDI (macOS) via `midir::create_virtual` (unsupported on Windows; not
a target).

---

## 10. Plugins v1

Each row is an **in-process adapter** in capmeshd (§4) that drives an external data-plane daemon.

| Kind | Data-plane daemon | Adapter (control protocol) | Notes |
|---|---|---|---|
| `midi` | **`nmidid`** (RTP-MIDI / AppleMIDI) | `nmidi-ctl` Unix socket (§4 flavor 2) | separate nmidi workstream extends it into a data-plane daemon + closes its two TODOs |
| `audio` | PipeWire + **roc** / PipeWire-RTP + AES67 | native — `pw-cli` / libpipewire | WirePlumber keeps links |
| `screen` | **Sunshine/Moonlight** / **wayvnc** | native — `wayvncctl` JSON-IPC / Sunshine | "mirror" + Cast-style "display URL" |
| `control-api` | **Moonraker** / **PrusaLink** | native — Moonraker WS JSON-RPC / PrusaLink HTTP | generic over both printers; `invoke` in MCP |
| `surface` | **`surfaced`** (HTTP/WS server, our own) | `surface-ctl` (capmesh-ctl + surface ops) | durable scriptable display sink; browser tab = attachment; see §10.1 |

---

## 10.1 The browser surface capability (`surface`)

A **surface** is a **durable, addressable, arbitrarily-scriptable display sink** on the mesh — the
"aim a browser at it and it becomes a screen you can push to" capability. The design turns on one
split and one unification.

**Split — the device is NOT the tab.** The logical surface lives in a small **`surfaced`
data-plane daemon** (an HTTP/WS server) and holds all state; it exists whether or not a browser is
open. An actual **browser tab is an ephemeral *attachment*** pointed at `http://<host>:<port>/s/
<surface-id>`, 0..N of them, connected back over WebSocket/SSE. On attach it syncs current state +
streams live updates; on close, the surface and its history live on. This is what makes "register a
browser and have it persist even when it's not open" real, and it's declared permanent in NixOS
(§9) like any durable device.

**Unification — a surface IS a persistent inbox.** Every push appends a **display item** to a
**durable, ordered, bounded inbox** the surface keeps (survives close, survives `surfaced` restart
via on-disk state). The tab renders **two zones**: a **main view** = the currently-selected item
(latest by default; the operator can scroll back and re-select any past item) and a **visible inbox
feed** listing everything ever pushed. So "just show the current thing" and "see everything that's
been pushed" are the same model — the current view is the head of the inbox, and the feed is its
history. A push may target the inbox only (a link that waits for you) or also promote to the main
view (a PDF the agent wants shown now).

**Arbitrarily scriptable — because `surfaced` serves *our own* page.** The attachment page is
same-origin to `surfaced`, so a display item can be as rich as **arbitrary HTML / JS / a live
component**, not just a URL. Built-in item types (structured, for convenience):

| item type | renders |
|---|---|
| `navigate {url}` | an **iframe** to a third-party URL (one thing among many) |
| `pdf {url}` | a PDF viewer (the "put this manual page on my phone" case) |
| `text {body}` / `link {url, title}` | a text note / a clickable link in the feed |
| `html {markup}` | arbitrary same-origin DOM we control |
| `script {code}` | arbitrary JS run in the surface page — full scripting escape hatch |

**Why arbitrary script is safe here:** pushes are gated by the **trust boundary** (§8) — only
cluster-authenticated mesh hosts / trusted agents can `send` to a surface — so "run arbitrary JS in
my display" is a *feature the surface trusts its controller with*, not an open hole. **Untrusted
third-party web content is the case we sandbox:** a random website/PDF is rendered inside an
**iframe** (isolated by same-origin policy), never injected as script. So: we fully script *our*
surface; we *contain* someone else's page.

**Attachment auth.** A browser opening `…/s/<id>?token=<t>` presents a **per-surface token**
(the surface's `send`-capability credential is separate). This suits a **phone** — not a
dotfiles-managed host, so it can't carry the cluster key — while keeping the surface itself a
trusted, mesh-advertised device. Path to a stronger per-device pairing later.

**Data-plane daemon:** `surfaced` is a first-party daemon (like `nmidid`) — it speaks an extended
`capmesh-ctl` (`surface-ctl`: `create-surface` / `list-surfaces` / `send-item` / `set-view` /
`list-items` / attach lifecycle) over the local control socket; capmeshd's `surface` adapter drives
it for **control/discovery** (`list-surfaces` is the adapter's discovery verb). Pushing items is a
data-plane action, so `surfaced` also hosts its **own** embedded MCP server (`list_surfaces` /
`list_items` / `send_item`) that agents call to push — the push verb is not on capmeshd's MCP (§7).
`surfaced` owns the HTTP/WS serving, the inbox store, attachment fan-out, and its data-plane MCP;
capmeshd stays stateless plumbing.

---

## 11. Phased execution plan (starts from nmidi)

M0 runs as **two coordinated workstreams** (two build verticals):

**M0a — nmidi → `nmidid` (a data-plane daemon with a control socket).** *Separate nmidi workstream.*
- Close nmidi's two `TODO`s (`nmidi-client/src/main.rs`): a proper connection **state machine**
  (Connecting → Connected → Disconnected/Rejected) and **MIDI mounting** — create a local virtual
  ALSA/CoreMIDI port + bidirectional RTP-MIDI forwarding (`midir::create_virtual`).
- Wrap those behind a **`nmidi-ctl` control socket** (`list-caps` / `mount {local-port, peer,
  descriptor}` / `unmount` / `status`) so nmidi becomes "purely in charge of the MIDI data plane."
- **Exit:** `nmidid` creates/tears-down a mount on a control-socket command; data flows nmidid↔nmidid.

**M0b — capmeshd MIDI slice (drives `nmidid`).** *capmeshd workstream.*
- Minimal capmeshd: `_capmesh._tcp` advertise + browse (lift nmidi's `ServiceAdvertiser`), the
  desired-mount reconciler, a `midi` adapter that speaks `nmidi-ctl`, and a minimal control API
  (`discover`/`connect`/`disconnect`/`status`, MIDI only) reading the §4.1 TOML.
- First NixOS module (`services.capmesh.advertise.midi`) rendering the TOML; deploy to two machines
  via Colmena.
- **Exit / demo:** a **physical MIDI device plugged into one host plays a SuperCollider instance on
  another host** (cross-OS CoreMIDI↔ALSA), wired from one command. This is the operator's milestone.

**M1 — Extract the generic abstraction + auto-mount.** Refactor capmeshd into `capmesh-discovery`
(generalized advertiser/browse over `_capmesh._tcp` + descriptor fetch), `capmesh-model` (the typed
node/port model §3.1 + descriptor + mount types), `capmesh-daemon` (reconcile loop + the in-process
`CapabilityPlugin` adapter trait). MIDI is the first adapter. Add **auto-mount selectors** (§6.1):
the reconciler derives desired mounts from discovery events. **Exit (the headline demo):** *plug a
MIDI keyboard into the laptop and it appears, unasked, as a virtual MIDI input on the desktop* — and
disappears when unplugged. A new kind = one adapter module + a §4.1 config entry.

**M2 — MCP server.** Wrap the control API in the §7 MCP server (Streamable HTTP). **Exit:** an agent
discovers and wires MIDI mounts over the mesh with no bespoke glue.

**M2.5 — Browser surface (agent-facing; operator-prioritized).** Build `surfaced` (§10.1): the
HTTP/WS server, the durable per-surface **inbox store** + on-disk persistence, the attachment page
(main view + visible inbox feed), `surface-ctl` over the control socket, capmeshd's `surface`
adapter (control/discovery only), `surfaced`'s own embedded MCP `send_item` tool (data-plane push),
per-surface token attach, and the `services.capmesh.advertise.surface`
NixOS bit. **Exit:** a voice/agent pushes a **PDF manual page to the phone surface** and it appears
(and stays in the phone's inbox, visible later even after the tab was closed). *This capability is
independent of the media legs; it can land right after the MCP server rather than waiting for M3–M5.*

**M3 — Audio.** `audio` plugin driving PipeWire + roc (resilient) and PipeWire-RTP/AES67 (interop).
**Exit:** "route host X's audio to the living-room speakers" via agent/command.

**M4 — Screen.** `screen` plugin over Sunshine/Moonlight and/or wayvnc; model both "mirror to X" and
Cast-style "display URL on X." **Exit:** "show the workshop laptop on the living-room TV" on the fly.

**M5 — Klipper / control-APIs.** `control-api` kind + a plugin that publishes the mDNS record
Moonraker lacks and proxies Moonraker WS JSON-RPC (and PrusaLink HTTP); expose `invoke` in MCP.
**Exit:** "auto-discover the Voron and start/pause/monitor a print" via agent.

**M6 — Hardening & interop.** Cluster key → optional mTLS/WireGuard underlay; ingest third-party
adverts (`_apple-midi`, `_octoprint`, Cast) as untrusted; optional Home Assistant adapter for the
pure-IoT slice.

---

## 12. Open decisions (chosen defaults)

| # | Decision | Options | **Default** |
|---|---|---|---|
| D1 | Distributed vs central | fully distributed / central hub / **split (control-view central, data+discovery distributed)** | **split** (§2) |
| D2 | Control model | imperative-only / **reconcile** / hybrid | **reconcile** (§6) — permanent mounts declarative, temp = TTL lease |
| D3 | Plugin location & data plane | separate mapper daemon per kind / **in-process TOML-driven adapters in capmeshd, data-plane daemons stay external** | **in-process adapters** (§4, §4.1); capmeshd stateless plumbing, drives data-plane daemons over control sockets, carries no data |
| D4 | Security v1 | open LAN / **cluster key via age** / mTLS now | **cluster key via age** (§8), mTLS path later |
| D5 | nmidi relationship | wrap standalone / **generalize into capmesh crates** | **generalize** (§4, §11-M1) |
| D6 | Discovery transport | central registry / **mDNS + descriptor pointer** | **mDNS + descriptor pointer** (§5) |
| D7 | Data-plane model | per-kind bespoke / **unified typed ports (`stream` source/sink + `rpc`)** | **unified typed ports** (§3.1) — control-APIs are `rpc` handles, not fake streams |
| D8 | Virtual endpoints | mount = bytes only / **transports may materialize local virtual endpoints** | **virtual endpoints** (§3.2) — remote device shows up as a native local device |
| D9 | Auto-mount | explicit connect only / **declarative discovery-driven selectors** | **auto-mount selectors** (§6.1), per-host opt-in, trust-gated — lands in M1 |
| D10 | Browser as a capability | ephemeral tab / **durable `surface` sink = persistent inbox, tab is an attachment** | **durable surface** (§10.1) — device ≠ tab; every push logged in a visible inbox that survives close |
| D11 | Surface scripting | fixed iframe+overlay only / **arbitrarily scriptable (our same-origin page)** | **arbitrary script** (§10.1) — safe because pushes are trust-gated; untrusted third-party pages sandboxed in an iframe |

---

## 13. Risks & honest caveats

- **Control planes accrete.** Keep the daemon thin; every capability's *data plane* stays a reused
  external transport, never reimplemented.
- **mDNS across VLANs/Wi-Fi is flaky.** Plan for an mDNS reflector or a small unicast-registry
  fallback for multi-subnet segments.
- **Cross-OS virtual ports.** `midir::create_virtual` works on ALSA + CoreMIDI, not Windows —
  fine for the current fleet.
- **nmidi is early.** M0 is real work (the mounting TODO is the whole mount primitive in miniature),
  but it is exactly the work that validates the abstraction before generalizing.
- **Don't rebuild Home Assistant** for the pure-IoT slice — adapt to it (M6).
- **The surface's arbitrary-script power leans entirely on the trust boundary (§8).** `send` MUST be
  cluster-authenticated; the per-surface attach token is for *rendering*, not for pushing. Untrusted
  web content is only ever iframe-sandboxed, never injected as script. If the trust boundary is ever
  weakened, revisit surface scripting first.
