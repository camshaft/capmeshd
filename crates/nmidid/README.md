# nmidid

The MIDI **data-plane daemon**. `nmidid` owns the RTP-MIDI data plane behind the
`capmesh-ctl` control socket; capmeshd (the control plane) drives it over a local
Unix domain socket carrying newline-delimited JSON-RPC 2.0. See
[`../../docs/CONTROL-PROTOCOL.md`](../../docs/CONTROL-PROTOCOL.md) for the wire
contract.

## Running the daemon

```sh
nmidid --socket /run/nmidid.sock --log-level info
```

| flag | default | meaning |
|---|---|---|
| `--socket <path>` | `/run/nmidid.sock` | Unix control socket to bind (§1). |
| `--monitor-interval <secs>` | `5` | How often to poll local MIDI ports for hot-plug (§5). |
| `--allow-uid <uid>` | *(none)* | Permit connections from this uid (repeatable); own uid always allowed (§1.1). |
| `--allow-gid <gid>` | *(none)* | Permit connections from this gid (repeatable) (§1.1). |
| `--allow-group <name>` | *(none)* | Permit connections from this group name (repeatable), resolved to a gid at startup (§1.1). |
| `--log-level <lvl>` | `info` | `trace`/`debug`/`info`/`warn`/`error`. |

The control socket is local-trust-only. With none of `--allow-uid` / `--allow-gid`
/ `--allow-group` given, peer-credential enforcement is **off** and only the
socket file permissions (`0o660`) apply. Given any, nmidid reads each peer's
socket credentials (`SO_PEERCRED`) on connect and refuses any peer whose uid and
group set are not on the allow-list (its own uid is always allowed); it fails
closed if the credentials can't be read (§1.1). The group check uses the peer's
**full** group set — its primary gid plus supplementary groups (read from
`/proc/<pid>/status`) — so authorizing a shared group works even when the client
holds it as a supplementary group (e.g. a systemd `DynamicUser`).

`--allow-group` resolves a group *name* to its gid from `/etc/group` at startup;
an unresolved name is a fatal error (fail closed, not open). Prefer it to a raw
gid, since an auto-allocated NixOS group has no gid known at evaluation time.

The NixOS module `services.nmidid` (see the flake) runs it as a systemd unit and
exposes `allowedGroups` / `allowedUids` / `allowedGids` for the same policy — set
`allowedGroups = [ "capmesh" ]` to let capmeshd's client reach the socket while
refusing others.

Two gates apply, in order. First the **file permissions**: the socket is `0o660`,
so a client running as a different user (e.g. capmeshd under a systemd
`DynamicUser`) can only *open* it if it shares the socket's group — set the
module's `socketGroup = "capmesh"` so the socket is group-owned by the shared
group. Then the **peer-credential** check (`allowedGroups`, above) decides whether
that connection is served. Leaving `socketGroup` unset runs nmidid as `root:root`,
so only root (or a same-user client) can connect.

### Control methods (capmeshd → daemon)

`hello` · `list-ports` · `describe-port` · `mount` · `unmount` · `mount-status`
(CONTROL-PROTOCOL.md §1–§3). `mount` currently implements the `mirror-source`
role: it creates a local virtual MIDI source and pumps RTP-MIDI from the remote
into it, driving the mount `connecting → active`.

### Notifications (daemon → capmeshd, unsolicited, §5)

`mount-state` (on every mount transition) · `port-added` / `port-removed` (local
MIDI hot-plug, gated behind the `hotplug-events` capability). Port ids are
name-derived (`<dir>-<slug(name)>`, e.g. `source-keystation-49e`) and stable
across sibling reindex (§2).

## `nmidi-fake-source` — synthetic RTP-MIDI source (headless rehearsal / CI)

A second binary in this crate that plays the AppleMIDI *invitee* (source) role a
real MIDI device would: it accepts an invitation, answers clock-sync, and streams
periodic RTP-MIDI notes. It lets the full mount data path
(`connecting → active → bytes-in`) run **without physical MIDI hardware**.

```sh
nmidi-fake-source --bind 127.0.0.1 --port 5008 --note-interval-ms 100
```

| flag | default | meaning |
|---|---|---|
| `--bind <addr>` | `0.0.0.0` | Address for the control + data sockets. |
| `--port <port>` | `5008` | Control port; the **data port is `port + 1`** (AppleMIDI convention). |
| `--note-interval-ms <ms>` | `500` | Cadence of emitted MIDI notes. |
| `--no-notes` | *(off)* | Accept + answer clock-sync but emit no MIDI (test clock-sync keep-alive). |
| `--reject` | *(off)* | Reject every invitation with `NO` (test the mount reject → `failed` path). |
| `--name <str>` | `nmidi-fake-source` | Session name in the invitation reply and the mDNS service. |
| `--no-advertise` | *(off)* | Suppress the `_apple-midi._udp` mDNS advertisement. |
| `--log-level <lvl>` | `info` | Log verbosity. |

It is single-peer (the most recent inviter is the active peer). By default it
advertises its data-plane control port over `_apple-midi._udp` (the standard
RTP-MIDI discovery record), so a browsing host can learn where to point a mount
without the port being hand-supplied.

### Rehearsal wiring

Because `midir::create_virtual` needs an ALSA sequencer, the end-to-end rehearsal
runs in a NixOS VM test with `boot.kernelModules = [ "snd-virmidi" ]` in the
guest (hardware-independent, CI-gatable). Inside the guest:

1. Start `nmidid` and `nmidi-fake-source --port 5008`.
2. Point a mount at the fake source:
   `mount { role: "mirror-source", local: { virtual: true },
   remote: { addr: "127.0.0.1", port: 5008, port-id: <a source id from
   nmidid list-ports> }, format: { codec: "midi1" } }`.
   > The mount's `remote.port-id` is validated against `nmidid`'s **local**
   > `list-ports`, so pass a real id — `snd-virmidi` provides several.
3. Watch `mount-state` reach `active` with `stats.bytes-in > 0`.

The `nmidi-fake-source` binary ships in the `nmidid` package's output
(`bin/nmidi-fake-source`), so a nixosTest can pull it into the guest closure.
