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

> **The one-line architecture:** `capmeshd` advertises typed capabilities over DNS-SD and
> reconciles declarative "mounts" by driving existing transports through **capability plugins**,
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
   │ capmeshd                 │◄───────────  _capmesh._tcp  ───────────►│ capmeshd                 │
   │  • advertise caps        │  (distributed discovery; zero-config)   │  • desired-mount set      │
   │  • plugin: midi/audio/…  │                                         │  • reconcile actual↔desired│
   └───────────┬──────────────┘                                        └────────────┬──────────────┘
               │        DATA PLANE — always DIRECT peer↔peer, per capability          │
               ▼        (RTP-MIDI / PipeWire+roc / AES67 / Sunshine|wayvnc / WS)       ▼
   physical MIDI device / PipeWire node / screen / Moonraker  ══════════════►  local virtual endpoint
```

- **Discovery = distributed (mDNS/DNS-SD).** No host registers to a hub. Daemons announce and
  browse `_capmesh._tcp`. Join/leave is zero-config — the plug-n-play property the operator wants.
- **Data plane = strictly peer-to-peer, direct.** A mount is always a direct host↔host connection.
  **No media ever transits a hub** → no bottleneck, no single point of failure for live wiring.
- **Control plane / MCP = a mesh *client*, not a hub.** The MCP server browses the mesh via mDNS
  and issues control RPCs to peer daemons. The agent gets **one endpoint**; the distributed model
  is preserved; if the MCP host dies, every existing mount keeps running because each sink daemon
  self-heals its own desired-state.

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

---

## 4. The plugin system

`capmeshd` owns the control plane; **every capability kind is a plugin** implementing one trait.
This is the operator's explicit design instinct, confirmed.

```rust
/// One implementation per capability kind. capmeshd loads a set of these.
trait CapabilityPlugin {
    fn kind(&self) -> CapabilityKind;

    /// Enumerate what this host currently offers/consumes for this kind
    /// (e.g. midir port scan). Feeds the advertiser + the descriptor endpoint.
    async fn scan_local(&self) -> Vec<Capability>;

    /// Full typed descriptor for a locally-advertised capability (served over
    /// the daemon's HTTP/RPC at the `descr` pointer from the TXT record).
    async fn describe(&self, cap: &CapabilityId) -> Descriptor;

    /// Realize a data-plane connection: source cap (remote) → local sink (or
    /// vice-versa). Returns a live MountHandle whose Drop tears the connection down.
    async fn mount(&self, spec: &MountSpec) -> Result<MountHandle>;

    /// Liveness / throughput / last-seen for a live mount.
    async fn status(&self, mount: &MountId) -> MountStatus;
}
```

**Two realization styles, both behind the same trait — this is the key generality:**

- **In-process** (MIDI): the plugin links `nmidi-core`, does the AppleMIDI `IN/OK/CK` handshake
  (already implemented), then **creates a local virtual ALSA/CoreMIDI port and pumps RTP-MIDI both
  ways** — closing nmidi's two `TODO`s directly in the mount path.
- **Drive an external daemon** (audio/screen/printer): the plugin shells out to / speaks the IPC of
  an existing service — `pw-cli`/libpipewire for audio, `wayvncctl` JSON-IPC or Sunshine for screen,
  Moonraker/PrusaLink WebSocket for the printer. `capmeshd` never carries that data itself.

Adding a new capability = implementing this one trait. `MountHandle: Drop` gives idempotent
teardown for free and makes the reconciler simple (drop the handle to converge toward "unmounted").

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

---

## 7. The MCP surface

Thin MCP server (Rust; `rmcp` / official SDK), **Streamable HTTP** transport (long-running daemon,
matches Home Assistant's proven pattern) + stdio for local dev. Runs as a mesh client (§2).

- **Tools**
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

Hosts today: Linux `gateway` / `green-machine` / `i7-machine`; macOS `camerons-mini` /
`camerons-work-mbp`. **The mesh is cross-OS from day one** — the MIDI plugin creates virtual ports
on both ALSA (Linux) and CoreMIDI (macOS) via `midir::create_virtual` (unsupported on Windows; not
a target).

---

## 10. Plugins v1

| Kind | Data-plane transport (composed) | Realization | Notes |
|---|---|---|---|
| `midi` | RTP-MIDI / AppleMIDI (**nmidi**) | in-process | closes nmidi's two TODOs; first plugin |
| `audio` | PipeWire + **roc** (resilient) / PipeWire-RTP + AES67 (interop) | drive external | WirePlumber keeps links |
| `screen` | **Sunshine/Moonlight** (media-grade) / **wayvnc** (`wayvncctl` IPC) | drive external | "mirror" + Cast-style "display URL" |
| `control-api` | **Moonraker** JSON-RPC/WS *and* **PrusaLink** HTTP | drive external | generic over both printers; `invoke` in MCP |

---

## 11. Phased execution plan (starts from nmidi)

**M0 — Finish the MIDI vertical on nmidi; prove the whole shape end-to-end.**
- Close nmidi's two `TODO`s (`nmidi-client/src/main.rs`): a proper connection **state machine**
  (Connecting → Connected → Disconnected/Rejected) and **MIDI mounting** — create a local virtual
  ALSA/CoreMIDI port + bidirectional RTP-MIDI forwarding.
- Add a minimal long-running control API (`discover`/`connect`/`disconnect`/`status`, MIDI only).
- First NixOS module (`services.capmesh.advertise.midi`), deploy to two machines via Colmena.
- **Exit / demo:** a **physical MIDI device plugged into one host plays a SuperCollider instance on
  another host** (cross-OS CoreMIDI↔ALSA), wired from one command. This is the operator's milestone.

**M1 — Extract the generic abstraction.** Refactor into `capmesh-discovery` (generalized
advertiser/browse over `_capmesh._tcp` + descriptor fetch), `capmesh-model` (capability descriptor +
mount types), `capmesh-daemon` (reconcile loop + the `CapabilityPlugin` trait). MIDI becomes the
first plugin. **Exit:** MIDI works unchanged through the generic daemon; a new kind = one trait impl.

**M2 — MCP server.** Wrap the control API in the §7 MCP server (Streamable HTTP). **Exit:** an agent
discovers and wires MIDI mounts over the mesh with no bespoke glue.

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
| D3 | Plugin realization | in-process only / external-daemon only / **both behind one trait** | **both** (§4) |
| D4 | Security v1 | open LAN / **cluster key via age** / mTLS now | **cluster key via age** (§8), mTLS path later |
| D5 | nmidi relationship | wrap standalone / **generalize into capmesh crates** | **generalize** (§4, §11-M1) |
| D6 | Discovery transport | central registry / **mDNS + descriptor pointer** | **mDNS + descriptor pointer** (§5) |

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
