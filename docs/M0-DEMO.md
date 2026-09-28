# M0 demo runbook — a MIDI source on one host plays SuperCollider on another

The M0 exit demo: a physical MIDI device attached to a **source host** shows up — unasked —
as a local virtual MIDI port on a **SuperCollider host**, so SuperCollider on the second
machine plays from the keyboard on the first. Cross-OS (CoreMIDI ↔ ALSA), wired declaratively.

This runbook is for the SuperCollider (Linux) host, which is the fully-shipped side. Its
co-deploy config is gate-checked by `checks.m0-demo-codeploy` in [`flake.nix`](../flake.nix), so
the snippet below stays honest.

## Topology

```
  source host                                  SuperCollider host (Linux)
  ┌──────────────────────────┐                 ┌────────────────────────────────────────┐
  │ MIDI keyboard            │   LAN (mDNS)    │ capmeshd  ── auto-mount ──►  nmidid       │
  │  → capmeshd (advertises  │ ◄─────────────► │  browses _capmesh._tcp                   │
  │    _capmesh._tcp midi/   │   RTP-MIDI      │  + _apple-midi._udp                      │
  │    source)               │ ◄─────────────► │           creates a local virtual        │
  │  → RTP-MIDI session      │   (data plane)  │           ALSA source ──► SuperCollider   │
  │    (_apple-midi._udp)    │                 │                                          │
  └──────────────────────────┘                 └────────────────────────────────────────┘
```

The source host must advertise two mDNS records that capmeshd correlates **by IP**:

- `_capmesh._tcp` — the capmesh control endpoint, TXT `cap=midi dir=source` plus a `descr`
  pointer to the typed descriptor (DESIGN §5). capmeshd fetches the descriptor over this
  endpoint (see [MESH-PROTOCOL.md](MESH-PROTOCOL.md)).
- `_apple-midi._udp` — the RTP-MIDI session. Its SRV port is the **control** port; the
  data-plane daemon derives the data port as `control + 1`. capmeshd passes the control port
  into the mount verbatim (never `+1`).

For dev and CI, `nmidi-fake-source` stands in for the source host's RTP-MIDI session (it is
shipped in `packages.nmidid`, alongside `nmidid`, as `bin/nmidi-fake-source`).

## The SuperCollider host config (Colmena / NixOS)

Both daemons run on this host and share the `capmesh` trust group (DESIGN §8/§9): capmeshd
(a `DynamicUser`) joins the group so it can open nmidid's `0660` control socket (the
file-permission gate), and nmidid admits peers in that group over `SO_PEERCRED` (the
peer-credential gate). Both gates point at the one group — set both, not one.

```nix
{ config, ... }:
{
  imports = [
    inputs.capmeshd.nixosModules.capmesh
    inputs.capmeshd.nixosModules.nmidid
  ];

  # MIDI data-plane daemon. Its socket is group-owned by the shared capmesh group, and it
  # admits peers in that group (the two §9 gates).
  services.nmidid = {
    enable = true;
    socket = "/run/nmidid/nmidid.sock";
    socketGroup = config.services.capmesh.group;      # file-permission gate (0660 socket)
    allowedGroups = [ config.services.capmesh.group ]; # peer-credential gate (SO_PEERCRED)
  };

  # Control plane. Advertises the local MIDI capability and auto-mounts any discovered remote
  # MIDI *source* as a local virtual source — the headline path, with no manual mount.
  services.capmesh = {
    enable = true;
    hostId = "sc-host";
    advertise.midi.enable = true;
    advertise.midi.socket = config.services.nmidid.socket;
    automount = [{
      match = { kind = "midi"; dir = "source"; };
      action = "mirror-local";      # materialize a local virtual source (alias of mirror-source)
      lifetime = "while-advertised"; # torn down automatically when the source stops advertising
    }];
  };
}
```

With this deployed, the demo needs **no command at all**: the moment the source host appears on
the LAN, capmeshd browses it, fetches the descriptor, correlates the control port from the
`_apple-midi._udp` record, and issues the mount to nmidid. SuperCollider sees a new ALSA source.
Unplug the source (its advert goes away) and the `while-advertised` mount is torn down.

## Driving it by hand (optional)

The same outcome without the `automount` rule, using the CLI (`SOCK=/run/nmidid/nmidid.sock`):

```sh
# See what's on the mesh (with typed descriptors):
capmeshd discover --timeout-secs 5

# Mount a capability by selector — capmeshd learns the address, port, control port, and codecs
# itself (no manual --remote-addr/--remote-port/--remote-port-id/--remote-codec):
capmeshd connect-discover --socket "$SOCK" --kind midi --role mirror-source

# Inspect / tear down:
capmeshd mount-status --socket "$SOCK"
capmeshd unmount --socket "$SOCK" --mount-id <id>
```

Narrow the selection with `--host <host-id>` or `--id <capability-id>` when more than one MIDI
source is present (`connect-discover` refuses an ambiguous match rather than guessing).

## Verifying the data-plane behavior (dev / CI stand-in)

Point a `nmidi-fake-source` at the host in place of a real source. Its knobs cover the
rehearsal assertions:

| flag              | behavior                          | asserts                                   |
|-------------------|-----------------------------------|-------------------------------------------|
| _(default)_       | streams notes                     | mount reaches `active`, bytes-in grows    |
| `--no-notes`      | accepts + clock-syncs, no notes   | stays `active` > session timeout, bytes-in == 0 (keep-alive) |
| `--reject`        | answers every invitation with NO  | mount fails fast, detail contains `rejected` |
| _(kill process)_  | stops responding                  | mount → `failed`, detail `unresponsive`   |

Removing the source's `_capmesh._tcp` advert mid-session exercises the `while-advertised`
teardown: capmeshd issues an unmount and the local virtual port disappears.

## Status

The SuperCollider-host side is complete and gate-checked (`nix flake check`). The remaining
M0-exit gate is the end-to-end CI rehearsal harness (a `nixosTest` with two guests / a virtual
MIDI source), which requires a KVM-capable venue to run.
