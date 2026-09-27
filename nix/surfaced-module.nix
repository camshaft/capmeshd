# The `services.surfaced` NixOS module — the browser-surface data-plane daemon
# (DESIGN §10.1). Runs `surfaced`, which serves durable surfaces over HTTP/SSE.
# A surface persists on disk (StateDirectory) so it survives a restart, and is
# declared permanent here like any durable device. Deployable from `dotfiles`
# via Colmena; companion to the `services.capmesh` control-plane module.
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.surfaced;
in
{
  options.services.surfaced = {
    enable = lib.mkEnableOption "the surfaced browser-surface data-plane daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.surfaced;
      defaultText = lib.literalExpression "surfaced flake package";
      description = "The surfaced package to run.";
    };

    address = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      example = "0.0.0.0";
      description = ''
        Address the HTTP/SSE server binds. Defaults to loopback; set to
        "0.0.0.0" (with openFirewall) to attach a phone/tablet from the LAN.
        Note: per-surface attach tokens are a later increment, so a LAN bind
        currently trusts everyone on the network — use only on a trusted LAN.
      '';
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8787;
      description = "TCP port the HTTP/SSE server binds.";
    };

    stateDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/surfaced";
      description = ''
        Directory for durable per-surface inbox logs. The default is managed by
        systemd StateDirectory; change it only alongside the service hardening.
      '';
    };

    basePath = lib.mkOption {
      type = lib.types.str;
      default = "";
      example = "/surfaced";
      description = ''
        Mount the server under a URL prefix for reverse-proxy deployment (nginx).
        Empty (default) serves at the root. Proxy WITHOUT stripping the prefix:
        `location /surfaced/ { proxy_pass http://127.0.0.1:8787; proxy_buffering off; }`.
      '';
    };

    socket = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = "/run/surfaced/surfaced.sock";
      description = ''
        Path of the local `surface-ctl` control socket capmeshd drives
        (docs/SURFACE-PROTOCOL.md). `null` (default) runs HTTP-only, with no mesh
        control plane. When set under `/run/surfaced` the systemd RuntimeDirectory
        provides the directory. The socket is owner/group `0o660` (local trust);
        for capmeshd to drive it, capmeshd's service must share surfaced's group —
        wired when the capmesh `surface` adapter lands (coordinate cross-service).
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the TCP port in the firewall (needed for LAN attachments).";
    };

    logLevel = lib.mkOption {
      type = lib.types.enum [ "trace" "debug" "info" "warn" "error" ];
      default = "info";
      description = "Log verbosity.";
    };
  };

  config = lib.mkIf cfg.enable {
    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];

    systemd.services.surfaced = {
      description = "surfaced browser-surface data-plane daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/surfaced"
          + " --http-addr ${cfg.address}:${toString cfg.port}"
          + " --state-dir ${cfg.stateDir}"
          + " --log-level ${cfg.logLevel}"
          + lib.optionalString (cfg.basePath != "") " --base-path ${cfg.basePath}"
          + lib.optionalString (cfg.socket != null) " --socket ${cfg.socket}";
        StateDirectory = "surfaced";
        # /run/surfaced for the control socket (harmless when socket is unset).
        RuntimeDirectory = "surfaced";
        Restart = "on-failure";
        RestartSec = 2;
        DynamicUser = true;
        # HTTP daemon: no special privileges needed.
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
      };
    };
  };
}
