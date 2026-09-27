# capmeshd

A LAN capability mesh: a per-host Rust daemon that advertises a machine's typed **capabilities**
(MIDI, audio, screen, printer/control-API, …) over DNS-SD and **mounts** remote capabilities
locally by driving the *existing* transport for that kind — a thin, typed, agent-driven control
plane over transports that already exist. Local-first. Controllable by AI agents over MCP.

> A **LAN-native, distributed IFTTT**: pipe anything to anything, on the fly.

- **Discovery** is distributed (mDNS/DNS-SD) — zero-config, plug-n-play.
- **Data plane** is strictly peer-to-peer — no media transits a hub.
- **Control / MCP** is a mesh *client* any host can run — one endpoint for the agent, no SPOF.
- **Trust boundary** = the hosts managed by [`dotfiles`](https://github.com/camshaft/dotfiles),
  gated by a cluster key provisioned via `age`.

## Design

See [`docs/DESIGN.md`](docs/DESIGN.md) for the full design: the capability/mount model, the plugin
system, the DNS-SD schema, the reconcile control plane, the MCP surface, the NixOS module, and the
phased plan from `nmidi` outward.

**First milestone (M0):** a physical MIDI device plugged into one host plays a SuperCollider
instance on another host, cross-OS (CoreMIDI ↔ ALSA), wired from one command — proving the whole
shape end-to-end. Built by closing [`nmidi`](https://github.com/camshaft/nmidi)'s two mounting
`TODO`s.

## Related repos

- [`camshaft/nmidi`](https://github.com/camshaft/nmidi) — MIDI-over-network seed (the first plugin).
- [`camshaft/dotfiles`](https://github.com/camshaft/dotfiles) — NixOS fleet (Colmena, Avahi); where
  the `services.capmesh` module and permanent mounts live.
- [`camshaft/printers`](https://github.com/camshaft/printers) — Voron / Klipper / Moonraker (the
  `control-api` plugin target); migrating into the single `dotfiles` flake.
