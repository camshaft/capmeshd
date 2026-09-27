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

  settings = {
    "host-id" = cfg.hostId;
    "advertise-port" = cfg.advertisePort;
  }
  // lib.optionalAttrs (cfg.clusterKeyFile != null) { "cluster-key-file" = cfg.clusterKeyFile; }
  // lib.optionalAttrs (enabledDataplanes != { }) { dataplane = enabledDataplanes; };

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

    advertise = lib.mkOption {
      type = lib.types.attrsOf advertiseModule;
      default = { };
      example = lib.literalExpression ''{ midi.enable = true; }'';
      description = "Capability kinds this host advertises and how to reach each data-plane daemon.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.etc."capmesh/capmesh.toml".source = configFile;

    systemd.services.capmesh = {
      description = "capmesh capability-mesh control plane";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" "avahi-daemon.service" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/capmeshd --config /etc/capmesh/capmesh.toml";
        Restart = "on-failure";
        RestartSec = 2;
        DynamicUser = true;
      };
    };
  };
}
