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
  };

  config = lib.mkIf cfg.enable {
    systemd.services.nmidid = {
      description = "nmidid MIDI data-plane daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "sound.target" ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/nmidid --socket ${cfg.socket} --log-level ${cfg.logLevel}";
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
