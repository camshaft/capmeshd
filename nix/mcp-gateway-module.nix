# The `services.mcp-gateway` NixOS module — the MCP gateway data-plane daemon (DESIGN §7.2).
#
# Runs `mcp-gateway`, the single agent-facing `/mcp` that federates the mesh's MCP servers behind
# one namespaced tool surface. Configured entirely from a `--config` TOML (fleet mandate seq-1377:
# no env vars). Deployable from `dotfiles` via Colmena; companion to the `services.capmesh`
# control-plane module (capmeshd drives live route changes over the gateway control socket in a
# later increment; this module renders the static-upstream floor).
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.mcp-gateway;
  tomlFormat = pkgs.formats.toml { };

  # Render an upstream option-set to the crate's `[[upstream]]` schema (kebab-case keys;
  # crates/mcp-gateway/src/config.rs uses deny_unknown_fields, so a drifted key is a hard error).
  renderUpstream = u:
    {
      id = u.id;
      url = u.url;
      transport = u.transport;
    }
    // lib.optionalAttrs (u.protocolRev != null) { "protocol-rev" = u.protocolRev; };

  settings = {
    bind = "${cfg.address}:${toString cfg.port}";
    log = cfg.logLevel;
  }
  // lib.optionalAttrs (cfg.upstreams != [ ]) { upstream = map renderUpstream cfg.upstreams; };

  configFile = tomlFormat.generate "mcp-gateway.toml" settings;

  upstreamModule = lib.types.submodule {
    options = {
      id = lib.mkOption {
        type = lib.types.str;
        description = "Stable upstream id — also the gateway's tools/list namespace prefix (no `__`).";
      };
      url = lib.mkOption {
        type = lib.types.str;
        example = "http://127.0.0.1:7001/mcp";
        description = "The upstream's Streamable-HTTP endpoint URL.";
      };
      transport = lib.mkOption {
        type = lib.types.str;
        default = "streamable-http";
        description = "Transport the gateway speaks to this upstream.";
      };
      protocolRev = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "2026-07-28";
        description = "MCP protocol revision the upstream negotiates, if known.";
      };
    };
  };
in
{
  options.services.mcp-gateway = {
    enable = lib.mkEnableOption "the MCP gateway data-plane daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.mcp-gateway;
      defaultText = lib.literalExpression "mcp-gateway flake package";
      description = "The mcp-gateway package to run.";
    };

    address = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      example = "0.0.0.0";
      description = ''
        Address the agent-facing `/mcp` binds. Defaults to loopback (front it with a reverse proxy);
        set to "0.0.0.0" (with openFirewall) to serve the LAN directly.
      '';
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8091;
      description = "TCP port the `/mcp` endpoint binds.";
    };

    logLevel = lib.mkOption {
      type = lib.types.enum [ "trace" "debug" "info" "warn" "error" ];
      default = "info";
      description = "Log verbosity, rendered into the `--config` TOML `log` key (seq-1377: NOT RUST_LOG / not an env var).";
    };

    upstreams = lib.mkOption {
      type = lib.types.listOf upstreamModule;
      default = [ ];
      example = lib.literalExpression ''[ { id = "board"; url = "http://127.0.0.1:7001/mcp"; } ]'';
      description = ''
        The MCP servers to federate at startup (the explicit floor). Each is connected
        (initialize + tools/list) and merged under its id as a `tools/list` namespace prefix.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the TCP port in the firewall (needed to serve the LAN directly).";
    };
  };

  config = lib.mkIf cfg.enable {
    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];

    # seq-1377: configured from a `--config` TOML file, never env vars.
    environment.etc."mcp-gateway/mcp-gateway.toml".source = configFile;

    systemd.services.mcp-gateway = {
      description = "mcp-gateway — federated agent-facing /mcp";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/mcp-gateway --config /etc/mcp-gateway/mcp-gateway.toml";
        Restart = "on-failure";
        RestartSec = 2;
        # Pure HTTP daemon: no privileges, no state. The unit name (mcp-gateway) has no static
        # group of the same name, so DynamicUser mints its transient user/group cleanly.
        DynamicUser = true;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
      };
    };
  };
}
