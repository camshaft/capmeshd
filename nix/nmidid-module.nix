# The `services.nmidid` NixOS module — the MIDI data-plane daemon (CONTROL-PROTOCOL.md).
#
# Runs `nmidid`, which serves the `capmesh-ctl` control socket that capmeshd's midi
# adapter drives. Deployable from `dotfiles` via Colmena. Companion to the
# `services.capmesh` control-plane module in ./module.nix.
{ self }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.nmidid;
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
      description = "Log verbosity.";
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
      example = lib.literalExpression "[ config.ids.gids.audio ]";
      description = ''
        Gids permitted to connect to the control socket, enforced via the peer's
        socket credentials (CONTROL-PROTOCOL §1.1). Setting a shared group here is
        the intended way to let capmeshd's client reach the socket while refusing
        everyone else. Empty (with `allowedUids`) leaves enforcement off.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.nmidid = {
      description = "nmidid MIDI data-plane daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "sound.target" ];
      serviceConfig = {
        ExecStart = lib.concatStringsSep " " (
          [ "${cfg.package}/bin/nmidid" "--socket" "${cfg.socket}" "--log-level" cfg.logLevel ]
          ++ lib.concatMap (uid: [ "--allow-uid" (toString uid) ]) cfg.allowedUids
          ++ lib.concatMap (gid: [ "--allow-gid" (toString gid) ]) cfg.allowedGids
        );
        RuntimeDirectory = "nmidid";
        Restart = "on-failure";
        RestartSec = 2;
        # Local-trust socket: owner/group only (CONTROL-PROTOCOL §1.1).
        UMask = "0117";
        SupplementaryGroups = [ "audio" ];
      };
    };
  };
}
