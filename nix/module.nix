# The `services.capmesh` NixOS module (DESIGN §9).
#
# Renders the §4.1 data-plane TOML from declarative options and runs `capmeshd`. Relies
# on the already-enabled Avahi for the mDNS substrate (DESIGN §5). Deployable from
# `dotfiles` via Colmena.
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.capmesh;
  tomlFormat = pkgs.formats.toml { };

  # Enabled advertise entries → `[dataplane.<kind>]` tables (§4.1).
  enabledDataplanes = lib.mapAttrs
    (_name: dp:
      { protocol = dp.protocol; }
      // lib.optionalAttrs (dp.socket != null) { socket = dp.socket; }
      // lib.optionalAttrs (dp.endpoint != null) { endpoint = dp.endpoint; }
    )
    (lib.filterAttrs (_n: dp: dp.enable) cfg.advertise);

  # A permanentMounts entry → a `[[permanent-mount]]` table (§9 → §4.1). Optional fields are
  # OMITTED when unset — `pkgs.formats.toml` cannot represent null, and the parser's
  # deny_unknown_fields is happy with absent optionals.
  renderPermanentMount = m:
    {
      role = m.role;
      codec = m.codec;
      remote = {
        addr = m.remote.addr;
        port = m.remote.port;
        "port-id" = m.remote.portId;
      } // lib.optionalAttrs (m.remote.host != null) { host = m.remote.host; };
    }
    // lib.optionalAttrs (m.mountId != null) { "mount-id" = m.mountId; }
    // lib.optionalAttrs (m.localName != null) { "local-name" = m.localName; };

  # An automount entry → a `[[automount]]` table (§6.1). The `match` selector fields are all
  # optional (a `None` field matches anything, §6.1), so an unset field is OMITTED — the parser's
  # `deny_unknown_fields` on the selector accepts an absent optional, and `pkgs.formats.toml`
  # cannot represent null. `match` itself is always emitted (the parser requires it) even when
  # every selector field is unset, in which case the rule matches every discovered capability.
  renderAutomount = a:
    {
      match =
        lib.optionalAttrs (a.match.kind != null) { kind = a.match.kind; }
        // lib.optionalAttrs (a.match.port != null) { port = a.match.port; }
        // lib.optionalAttrs (a.match.dir != null) { dir = a.match.dir; };
      action = a.action;
    }
    // lib.optionalAttrs (a.lifetime != null) { lifetime = a.lifetime; };

  settings = {
    "host-id" = cfg.hostId;
    "advertise-port" = cfg.advertisePort;
  }
  // lib.optionalAttrs (cfg.clusterKeyFile != null) { "cluster-key-file" = cfg.clusterKeyFile; }
  // lib.optionalAttrs (enabledDataplanes != { }) { dataplane = enabledDataplanes; }
  // lib.optionalAttrs (cfg.automount != [ ]) {
    automount = map renderAutomount cfg.automount;
  }
  // lib.optionalAttrs (cfg.permanentMounts != [ ]) {
    "permanent-mount" = map renderPermanentMount cfg.permanentMounts;
  };

  configFile = tomlFormat.generate "capmesh.toml" settings;

  advertiseModule = lib.types.submodule ({ ... }: {
    options = {
      enable = lib.mkEnableOption "advertising this capability kind over the mesh";
      protocol = lib.mkOption {
        type = lib.types.str;
        default = "nmidi-ctl";
        description = "The data-plane adapter protocol (DESIGN §4.1).";
      };
      socket = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Unix-socket path for socket protocols (e.g. nmidid).";
      };
      endpoint = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Endpoint URL for endpoint protocols (e.g. Moonraker WS).";
      };
    };
  });

  automountModule = lib.types.submodule ({ ... }: {
    options = {
      match = lib.mkOption {
        description = ''
          Selector over discovered capabilities (§6.1): each set field must equal the
          corresponding attribute of a discovered capability; an unset field matches anything.
          `kind` and `dir` are known from the coarse advert; `port` (a port-id) is only known
          after the peer's descriptor is fetched, so a rule that pins `port` matches only once
          the descriptor resolves.
        '';
        default = { };
        type = lib.types.submodule {
          options = {
            kind = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Capability kind to match (e.g. `midi`); unset matches any kind.";
            };
            port = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Port-id to match against the fetched descriptor; unset matches any port.";
            };
            dir = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Direction to match (`source | sink | duplex`); unset matches any direction.";
            };
          };
        };
      };
      action = lib.mkOption {
        type = lib.types.enum [ "mirror-source" "mirror-sink" "mirror-local" "link" ];
        description = ''
          What the daemon does with a matching capability (§6.1). `mirror-source` (alias
          `mirror-local`) materializes a local virtual source, `mirror-sink` a local virtual
          sink, and `link` binds an existing local port.
        '';
      };
      lifetime = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "while-advertised";
        description = "How long the derived mount lives (e.g. `while-advertised`); unset uses the daemon default.";
      };
    };
  });

  permanentMountModule = lib.types.submodule ({ ... }: {
    options = {
      mountId = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Idempotency key (§3.1); defaults from the remote endpoint when unset.";
      };
      role = lib.mkOption {
        type = lib.types.enum [ "mirror-source" "mirror-sink" "link" ];
        default = "mirror-source";
        description = "Which end the daemon materializes (§3.1).";
      };
      localName = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Display name for the local virtual device (mirror roles).";
      };
      codec = lib.mkOption {
        type = lib.types.str;
        default = "midi1";
        description = "Chosen wire-format codec.";
      };
      remote = lib.mkOption {
        description = "The remote endpoint to mount — connect by IP, never a .local name (§5).";
        type = lib.types.submodule {
          options = {
            host = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
              description = "Remote host-id (informational).";
            };
            addr = lib.mkOption {
              type = lib.types.str;
              description = "Remote peer IP from the mDNS record (never a .local/.lan name).";
            };
            port = lib.mkOption {
              type = lib.types.port;
              description = "Remote data-plane port.";
            };
            portId = lib.mkOption {
              type = lib.types.str;
              description = "Remote port-id to mount.";
            };
          };
        };
      };
    };
  });
in
{
  options.services.capmesh = {
    enable = lib.mkEnableOption "the capmesh capability-mesh control plane";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "capmeshd flake package";
      description = "The capmeshd package to run.";
    };

    hostId = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      defaultText = lib.literalExpression "config.networking.hostName";
      description = "This host's stable mesh id.";
    };

    advertisePort = lib.mkOption {
      type = lib.types.port;
      default = 7420;
      description = "The control endpoint port advertised over _capmesh._tcp.";
    };

    clusterKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Path to the cluster credential (DESIGN §8 trust boundary).";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "capmesh";
      description = ''
        Shared local group that gates access to first-party data-plane control sockets
        (DESIGN §8). capmeshd's (DynamicUser) process joins this group via SupplementaryGroups,
        and a data-plane daemon (e.g. nmidid) admits only peers in it via SO_PEERCRED. The group
        is declared here so it exists on the host; authorize it BY NAME on the data-plane daemon
        in the deployment (e.g. `services.nmidid.allowedGroups = [ config.services.capmesh.group ]`)
        to turn peer-cred enforcement on. The group name is the coordination point — an
        auto-allocated group has no eval-time gid to pin.
      '';
    };

    advertise = lib.mkOption {
      type = lib.types.attrsOf advertiseModule;
      default = { };
      example = lib.literalExpression ''{ midi.enable = true; }'';
      description = "Capability kinds this host advertises and how to reach each data-plane daemon.";
    };

    automount = lib.mkOption {
      type = lib.types.listOf automountModule;
      default = [ ];
      example = lib.literalExpression ''
        [ { match.kind = "midi"; match.dir = "source"; action = "mirror-local";
            lifetime = "while-advertised"; } ]'';
      description = ''
        Auto-mount rules (DESIGN §6.1): the daemon evaluates every rule against each discovered,
        descriptor-fetched capability and issues the derived mount. This is the declarative form
        of the mesh's headline path — a matching source shows up as a local virtual device with
        no manual mount.
      '';
    };

    permanentMounts = lib.mkOption {
      type = lib.types.listOf permanentMountModule;
      default = [ ];
      example = lib.literalExpression ''
        [ { localName = "studio keyboard";
            remote = { addr = "192.168.1.23"; port = 5004; portId = "kbd-0"; }; } ]'';
      description = "Permanent desired mounts reconciled every boot for self-heal (DESIGN §9).";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.etc."capmesh/capmesh.toml".source = configFile;

    # The shared local trust-boundary group (DESIGN §8). Declared with no explicit gid so it
    # merges cleanly when a data-plane daemon module (e.g. services.nmidid) declares it too.
    users.groups.${cfg.group} = { };

    systemd.services.capmesh = {
      description = "capmesh capability-mesh control plane";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" "avahi-daemon.service" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/capmeshd --config /etc/capmesh/capmesh.toml";
        Restart = "on-failure";
        RestartSec = 2;
        # Keep the DynamicUser sandbox; join the shared capmesh group so the data-plane
        # daemon's SO_PEERCRED gid check (DESIGN §8) admits this client once enforcement is on.
        DynamicUser = true;
        SupplementaryGroups = [ cfg.group ];
      };
    };
  };
}
