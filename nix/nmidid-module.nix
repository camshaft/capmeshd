# The `services.nmidid` NixOS module — the MIDI data-plane daemon (CONTROL-PROTOCOL.md).
#
# Runs `nmidid`, which serves the `capmesh-ctl` control socket that capmeshd's midi
# adapter drives. Deployable from `dotfiles` via Colmena. Companion to the
# `services.capmesh` control-plane module in ./module.nix.
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.nmidid;
  tomlFormat = pkgs.formats.toml { };

  # The §1/§1.1 config rendered from options into nmidid's --config TOML (fleet mandate seq-1377:
  # daemons take config from a file, never env vars). Keys are nmidid's shipped schema (kebab-case,
  # serde deny_unknown_fields — an unknown/typo'd key is a hard parse error). socketGroup is a
  # systemd concern (the unit's Group=), not a crate config key, so it is NOT in the TOML.
  settings = {
    socket = toString cfg.socket;
    "monitor-interval-secs" = cfg.monitorInterval;
    log = cfg.logLevel;
    "allow-uids" = cfg.allowedUids;
    "allow-gids" = cfg.allowedGids;
    "allow-groups" = cfg.allowedGroups;
  };
  configFile = tomlFormat.generate "nmidid.toml" settings;
in
{
  options.services.nmidid = {
    enable = lib.mkEnableOption "the nmidid MIDI data-plane daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.nmidid;
      defaultText = lib.literalExpression "nmidid flake package";
      description = "The nmidid package to run.";
    };

    socket = lib.mkOption {
      type = lib.types.path;
      default = "/run/nmidid/nmidid.sock";
      description = "Path of the Unix control socket nmidid binds (CONTROL-PROTOCOL §1).";
    };

    logLevel = lib.mkOption {
      type = lib.types.enum [ "trace" "debug" "info" "warn" "error" ];
      default = "info";
      description = ''
        Log verbosity, rendered into the TOML `log` key (nmidid reads it from `--config`, not from
        `RUST_LOG` or any env var; fleet mandate seq-1377).
      '';
    };

    monitorInterval = lib.mkOption {
      type = lib.types.int;
      default = 5;
      description = ''
        Hot-plug poll interval in seconds (CONTROL-PROTOCOL §5), rendered into the TOML
        `monitor-interval-secs` key.
      '';
    };

    socketGroup = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "capmesh";
      description = ''
        Group that owns the control socket (and its runtime directory), set as the
        service's primary group. The socket is mode `0660`, so a client running as
        a different user (e.g. capmeshd under a systemd `DynamicUser`) can only
        open it if it shares this group. Set to the shared `capmesh` group — the
        one `services.capmesh` declares and capmeshd joins via `SupplementaryGroups`
        — so co-located capmeshd can reach the socket. This is the file-permission
        gate that precedes the peer-credential check (`allowedGroups`, §1.1). When
        null, nmidid runs as `root:root` and only root (or a same-user client) can
        connect. The group must exist (declared elsewhere).
      '';
    };

    allowedUids = lib.mkOption {
      type = lib.types.listOf lib.types.int;
      default = [ ];
      example = lib.literalExpression "[ 1000 ]";
      description = ''
        Uids permitted to connect to the control socket, enforced via the peer's
        socket credentials (CONTROL-PROTOCOL §1.1). The daemon's own uid is always
        allowed. When both this and `allowedGids` are empty, peer-credential
        enforcement is off and only the socket file permissions apply.
      '';
    };

    allowedGids = lib.mkOption {
      type = lib.types.listOf lib.types.int;
      default = [ ];
      example = lib.literalExpression "[ 29 ]";
      description = ''
        Gids permitted to connect to the control socket, enforced via the peer's
        socket credentials (CONTROL-PROTOCOL §1.1). Prefer `allowedGroups` for the
        shared-group model — an auto-allocated NixOS group has no gid known at
        evaluation time. Empty (with `allowedUids`/`allowedGroups`) leaves
        enforcement off.
      '';
    };

    allowedGroups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = lib.literalExpression ''[ "capmesh" ]'';
      description = ''
        Group NAMES permitted to connect to the control socket, resolved to gids
        from `/etc/group` at daemon startup (CONTROL-PROTOCOL §1.1). This is the
        recommended way to authorize capmeshd: set `[ "capmesh" ]` — the shared
        group that `services.capmesh` declares and that capmeshd's client joins
        via `SupplementaryGroups`. Coordinating on the name avoids pinning a gid.
        The named group must exist (declared elsewhere); nmidid refuses to start
        if it cannot be resolved. Empty leaves enforcement off (unless other
        allow-lists are set).
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Render nmidid's config to /etc so the unit launches from `--config` (seq-1377: no argv/env
    # config). deny_unknown_fields on the crate side means a render/key drift is a hard startup error.
    environment.etc."nmidid/nmidid.toml".source = configFile;

    systemd.services.nmidid = {
      description = "nmidid MIDI data-plane daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "sound.target" ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/nmidid --config /etc/nmidid/nmidid.toml";
        RuntimeDirectory = "nmidid";
        # Owner + group only: with `socketGroup` set, only root and that group can
        # traverse to the socket; others cannot even see it (the socket itself is
        # 0660, so this is defense-in-depth on the local-trust boundary, §1.1).
        RuntimeDirectoryMode = "0750";
        Restart = "on-failure";
        RestartSec = 2;
        # Local-trust socket: owner/group only (CONTROL-PROTOCOL §1.1).
        UMask = "0117";
        SupplementaryGroups = [ "audio" ];
        # Group-own the socket (and RuntimeDirectory) so a co-located client in
        # this group can open the 0660 socket — the file-permission gate that
        # precedes the peer-credential check. `audio` stays available as a
        # supplementary group for the virtual-MIDI devices.
      } // lib.optionalAttrs (cfg.socketGroup != null) {
        Group = cfg.socketGroup;
      };
    };
  };
}
