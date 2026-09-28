# capmeshd

A LAN capability mesh: a per-host Rust daemon that advertises a machine's typed **capabilities**
(MIDI, audio, screen, printer/control-API, …) over DNS-SD and **mounts** remote capabilities
locally by driving the *existing* transport for that kind — a thin, typed, agent-driven control
plane over transports that already exist. Local-first. Controllable by AI agents over MCP.

> A **LAN-native, distributed IFTTT**: pipe anything to anything, on the fly.

- **`capmeshd`** is **stateless control-plane plumbing** — in-process, TOML-driven adapters drive
  each capability's own data-plane daemon over a control socket; it carries no data itself.
- **Discovery** is distributed (mDNS/DNS-SD) — zero-config, plug-n-play.
- **Data plane** is strictly peer-to-peer, daemon↔daemon — no media transits a hub or capmeshd.
- **Control / MCP** is a mesh *client* any host can run — one endpoint for the agent, no SPOF.
- **Trust boundary** = the hosts managed by [`dotfiles`](https://github.com/camshaft/dotfiles),
  gated by a cluster key provisioned via `age`.

## Monorepo layout

This repository is the single monorepo for the first-party capability-mesh daemons. Each is a
workspace crate under `crates/`:

- **`capmeshd`** — the stateless control-plane daemon and its CLI (this crate). Advertises/browses
  `_capmesh._tcp`, reconciles desired mounts, and drives each data-plane daemon over its control
  socket.
- **`nmidi-core`** / **`nmidid`** — the MIDI data-plane daemon and its shared core. `nmidid` serves
  the capmesh-ctl control socket capmeshd's MIDI adapter speaks to, and moves the MIDI bytes p2p
  (RTP-MIDI/AppleMIDI) over CoreMIDI (macOS) / ALSA (Linux). Consolidated in from the former
  standalone `camshaft/nmidi`.
- **`surfaced`** — the browser-surface data-plane daemon (DESIGN §10.1): durable, scriptable
  display sinks.

The flake exposes a package per daemon (`packages.{capmeshd,nmidid,surfaced}`) and a NixOS module
per daemon (`nixosModules.{capmesh,nmidid,surfaced}`); see **Deploy** below.

## Design

See [`docs/DESIGN.md`](docs/DESIGN.md) for the full design: the capability/mount model, the plugin
system, the DNS-SD schema, the reconcile control plane, the MCP surface, the NixOS module, and the
phased plan from MIDI outward. The daemon↔daemon control wire is frozen in
[`docs/CONTROL-PROTOCOL.md`](docs/CONTROL-PROTOCOL.md) (capmesh-ctl).

**First milestone (M0):** a physical MIDI device plugged into one host plays a SuperCollider
instance on another host, cross-OS (CoreMIDI ↔ ALSA), wired from one command — or, with an
`automount` rule, appearing **unasked** as the source comes on the LAN. See the
[M0 demo runbook](docs/M0-DEMO.md) for the deployable config and the step-by-step. The
peer-to-peer descriptor-fetch wire (how a browser resolves a coarse advert into a typed
capability) is [`docs/MESH-PROTOCOL.md`](docs/MESH-PROTOCOL.md).

## Usage

`capmeshd` runs as a daemon (advertise + browse + reconcile permanent mounts) when invoked with no
subcommand; the subcommands are one-shot control operations against a data-plane daemon's
capmesh-ctl socket. Run `capmeshd <command> --help` for the full flag list.

```
capmeshd [--config <PATH>] [--port <PORT>] [COMMAND]
```

Global options: `--config` (path to the §4.1 TOML, default `/etc/capmesh/capmesh.toml`), `--port`
(override the advertised control-endpoint port).

| Command | What it does |
| --- | --- |
| *(none)* | Run the daemon: advertise this host's capabilities, browse the mesh, and reconcile the config's permanent mounts. |
| `discover` | Browse the mesh and list discovered capabilities (§7). Filter with `--kind` / `--host`; `--dir` filters on the descriptor's real ports; `--timeout-secs` bounds the browse. |
| `connect` | Negotiate a format then mount a remote source onto a local mount in one step (§4, §7), given the remote coordinates (`--remote-addr`/`--remote-port`/`--remote-port-id`) explicitly. |
| `connect-discover` | Discover a capability by `--kind`/`--host`/`--id` and mount it in one command — learns the address, control port, port, and codecs itself (§6.1/§7). The port direction defaults from `--role`; `--dir`/`--port` override it. |
| `mount` | Establish a mount on a daemon (§3): create/attach a p2p link to a remote port. |
| `unmount` | Tear a mount down by `--mount-id` (§3). |
| `mount-status` | Print live mounts on a daemon (§3); restrict with `--mount-id`. |
| `reconcile` | Reconcile one desired mount against a daemon (DESIGN §6): issue it if absent/failed, leave it if live. `--interval-secs` polls; `--watch` reacts to §5 notifications event-driven. |
| `probe-ctl` | Connect to a data-plane daemon's socket, run hello + list-ports, and print what it reports — the MIDI adapter's integration probe against `nmidid`. `--watch` keeps printing notifications. |

Peers are always addressed by the **IP from the mDNS record** (`--remote-addr`), never a
`.local`/`.lan` name (DESIGN §5).

Probe a running `nmidid` and mount a discovered source:

```sh
# See what a data-plane daemon exposes over its control socket.
capmeshd probe-ctl --socket /run/nmidid.sock

# Discover MIDI sources on the mesh.
capmeshd discover --kind midi --dir source

# Discover + mount a capability in one command — no manual coordinates (learns addr, control
# port, port, and codecs itself; binds the source port for a mirror-source role).
capmeshd connect-discover --socket /run/nmidid.sock --kind midi --role mirror-source

# Or mount a remote source by explicit coordinates (addr is the peer's mDNS IP, never a .local name).
capmeshd mount --socket /run/nmidid.sock \
  --remote-host green-machine --remote-addr 192.168.1.23 \
  --remote-port 5004 --remote-port-id kbd-0

# Keep it converged: re-mount on drift, event-driven on daemon notifications.
capmeshd reconcile --socket /run/nmidid.sock \
  --remote-host green-machine --remote-addr 192.168.1.23 \
  --remote-port 5004 --remote-port-id kbd-0 --watch
```

## Deploy (NixOS)

The flake ships a `services.capmesh` module (`nixosModules.capmesh`, also `.default`). It renders
the §4.1 TOML declaratively and runs `capmeshd` as a systemd service. `automount` rules mount any
matching discovered capability with no manual step (§6.1, the headline path); `permanentMounts` are
reconciled every boot for self-heal (§9). Pair it with `services.nmidid` on hosts that serve MIDI —
wire both §8 trust gates to the shared group (`socketGroup` + `allowedGroups`). Deployable from
[`dotfiles`](https://github.com/camshaft/dotfiles) via Colmena. See the
[M0 demo runbook](docs/M0-DEMO.md) for the full co-deploy config.

```nix
services.capmesh = {
  enable = true;
  # hostId defaults to networking.hostName
  clusterKeyFile = "/run/agenix/capmesh-cluster.key";   # DESIGN §8 trust boundary
  advertise.midi = {
    enable = true;
    socket = "/run/nmidid.sock";                          # the nmidid control socket
  };
  # Auto-mount any discovered remote MIDI source as a local virtual source — the headline
  # path, no manual mount; torn down when the source stops advertising (§6.1).
  automount = [{
    match = { kind = "midi"; dir = "source"; };
    action = "mirror-local";
    lifetime = "while-advertised";
  }];
  # Desired mounts, reconciled on every boot — connect by IP, never a .local name (§5).
  permanentMounts = [{
    localName = "studio keyboard";
    remote = { addr = "192.168.1.23"; port = 5004; portId = "kbd-0"; };
  }];
};
```

## Related repos

- [`camshaft/dotfiles`](https://github.com/camshaft/dotfiles) — NixOS fleet (Colmena, Avahi); where
  the `services.capmesh` module and permanent mounts are wired into real hosts.
- [`camshaft/printers`](https://github.com/camshaft/printers) — Voron / Klipper / Moonraker (the
  `control-api` plugin target); migrating into the single `dotfiles` flake.

> The former standalone [`camshaft/nmidi`](https://github.com/camshaft/nmidi) is **deprecated** and
> consolidated into this monorepo as the `nmidi-core` / `nmidid` crates.
