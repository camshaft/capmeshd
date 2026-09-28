{
  description = "capmeshd — LAN capability mesh control-plane daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      perSystem = flake-utils.lib.eachDefaultSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };

          capmeshd = pkgs.rustPlatform.buildRustPackage {
            pname = "capmeshd";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            # Scope to the capmeshd crate only — do NOT build the whole workspace. This
            # decouples this package (which links no system libraries) from sibling
            # daemon crates (e.g. the incoming `nmidid`, which links ALSA/CoreMIDI via
            # midir and needs its own buildInputs in packages.nmidid). Adding such a
            # member no longer turns `nix flake check` red here.
            cargoBuildFlags = [ "-p" "capmeshd" ];
            cargoTestFlags = [ "-p" "capmeshd" "-p" "capmesh-discovery" "-p" "capmesh-model" "-p" "capmesh-ctl" "-p" "capmesh-daemon" "-p" "capmesh-mesh" ];
            # doCheck (default true) runs `cargo test` for capmeshd and its capmesh-{discovery,
            # model,ctl,daemon,mesh} libraries in the sandbox — the discovery TXT-schema, ctl
            # model-parse + socket round-trips, reconciler/negotiation, mesh HTTP fetch (loopback
            # TCP), and config-parser tests are self-contained.
            meta = {
              description = "LAN capability mesh: stateless control-plane daemon (MIDI-first)";
              license = pkgs.lib.licenses.mit;
              mainProgram = "capmeshd";
            };
          };

          # The MIDI data-plane daemon crate. Scoped to `-p nmidid`; midir links ALSA on
          # Linux and the CoreMIDI/CoreFoundation frameworks on macOS (pkg-config resolves
          # alsa.pc). doCheck runs the pure dispatch/framing unit tests in the sandbox.
          nmidid = pkgs.rustPlatform.buildRustPackage {
            pname = "nmidid";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "-p" "nmidid" ];
            cargoTestFlags = [ "-p" "nmidid" ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.alsa-lib ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ pkgs.apple-sdk ];
            meta = {
              description = "MIDI data-plane daemon — serves the capmesh-ctl control socket";
              license = pkgs.lib.licenses.mit;
              mainProgram = "nmidid";
            };
          };

          # The browser-surface data-plane daemon crate (DESIGN §10.1). Pure
          # Rust (axum/tokio/hyper) — links no system libraries, so no
          # buildInputs. doCheck runs the pure store/http unit tests (in-memory
          # + tempfile, no network) in the sandbox.
          surfaced = pkgs.rustPlatform.buildRustPackage {
            pname = "surfaced";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [ "-p" "surfaced" ];
            cargoTestFlags = [ "-p" "surfaced" ];
            meta = {
              description = "Browser surface data-plane daemon — durable scriptable display sinks";
              license = pkgs.lib.licenses.mit;
              mainProgram = "surfaced";
            };
          };
        in
        {
          packages.default = capmeshd;
          packages.capmeshd = capmeshd;
          packages.nmidid = nmidid;
          packages.surfaced = surfaced;

          checks.clippy = capmeshd.overrideAttrs (old: {
            pname = "${old.pname}-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
            # Lint capmeshd and its capmesh-{discovery,model,ctl,daemon,mesh} libraries together.
            buildPhase = "cargo clippy -p capmeshd -p capmesh-discovery -p capmesh-model -p capmesh-ctl -p capmesh-daemon -p capmesh-mesh --all-targets --release -- -D warnings";
            installPhase = "touch $out";
            doCheck = false;
          });

          # nmidid's own gate coverage: clippy over the daemon crate (owned by v-nmidid).
          checks.nmidid-clippy = nmidid.overrideAttrs (old: {
            pname = "${old.pname}-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy -p nmidid --all-targets --release -- -D warnings";
            installPhase = "touch $out";
            doCheck = false;
          });

          # surfaced's own gate coverage: clippy over the daemon crate (owned by v-surfaced).
          checks.surfaced-clippy = surfaced.overrideAttrs (old: {
            pname = "${old.pname}-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy -p surfaced --all-targets --release -- -D warnings";
            installPhase = "touch $out";
            doCheck = false;
          });

          # Prove the services.surfaced module's enabled path evaluates and renders
          # a systemd unit that runs surfaced with a durable state dir.
          checks.surfaced-module-eval =
            let
              sys = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.surfaced
                  ({ ... }: {
                    boot.loader.grub.enable = false;
                    fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
                    system.stateVersion = "24.11";
                    services.surfaced = {
                      enable = true;
                      address = "0.0.0.0";
                      port = 8787;
                      basePath = "/surfaced";
                      socket = "/run/surfaced/surfaced.sock";
                      openFirewall = true;
                    };
                  })
                ];
              };
              execStart = toString sys.config.systemd.services.surfaced.serviceConfig.ExecStart;
            in
            pkgs.runCommand "surfaced-module-eval" { inherit execStart; } ''
              printf '%s\n' "$execStart" | tee exec
              grep -q 'bin/surfaced' exec
              grep -q -- '--http-addr 0.0.0.0:8787' exec
              grep -q -- '--state-dir /var/lib/surfaced' exec
              grep -q -- '--base-path /surfaced' exec
              grep -q -- '--socket /run/surfaced/surfaced.sock' exec
              cp exec $out
            '';

          # Prove the NixOS module's enabled path evaluates and renders a §4.1 TOML that
          # the parser's `deny_unknown_fields` accepts (built by cargo test above).
          checks.nixos-module-render =
            let
              sys = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.default
                  ({ ... }: {
                    boot.loader.grub.enable = false;
                    fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
                    system.stateVersion = "24.11";
                    services.capmesh = {
                      enable = true;
                      hostId = "check-host";
                      clusterKeyFile = "/run/agenix/capmesh-cluster.key";
                      advertise.midi = {
                        enable = true;
                        socket = "/run/nmidid.sock";
                      };
                      automount = [{
                        match = { kind = "midi"; dir = "source"; };
                        action = "mirror-local";
                        lifetime = "while-advertised";
                      }];
                      permanentMounts = [{
                        localName = "studio keyboard";
                        remote = {
                          addr = "192.168.1.23";
                          port = 5004;
                          portId = "kbd-0";
                        };
                      }];
                    };
                  })
                ];
              };
              # The capmeshd service must join the shared trust-boundary group (DESIGN §8)
              # while keeping the DynamicUser sandbox, and the group must be declared.
              supGroups = toString (sys.config.systemd.services.capmesh.serviceConfig.SupplementaryGroups or [ ]);
              dynUser = pkgs.lib.boolToString (sys.config.systemd.services.capmesh.serviceConfig.DynamicUser or false);
              hasGroup = pkgs.lib.boolToString (sys.config.users.groups ? capmesh);
            in
            pkgs.runCommand "capmesh-module-render"
              {
                inherit supGroups dynUser hasGroup;
              } ''
              cp ${sys.config.environment.etc."capmesh/capmesh.toml".source} rendered.toml
              cat rendered.toml
              grep -q 'host-id = "check-host"' rendered.toml
              grep -q 'protocol = "nmidi-ctl"' rendered.toml
              grep -q 'socket = "/run/nmidid.sock"' rendered.toml
              grep -q '\[\[permanent-mount\]\]' rendered.toml
              grep -q 'port-id = "kbd-0"' rendered.toml
              grep -q 'addr = "192.168.1.23"' rendered.toml
              # §6.1 auto-mount rule: the [[automount]] table with its match selector + action.
              grep -q '\[\[automount\]\]' rendered.toml
              grep -q 'action = "mirror-local"' rendered.toml
              grep -q 'kind = "midi"' rendered.toml
              grep -q 'lifetime = "while-advertised"' rendered.toml
              # §8 trust boundary: capmesh group joined, group declared, sandbox kept.
              printf 'SupplementaryGroups=%s DynamicUser=%s hasGroup=%s\n' "$supGroups" "$dynUser" "$hasGroup"
              [ "$supGroups" = "capmesh" ]
              [ "$dynUser" = "true" ]
              [ "$hasGroup" = "true" ]
              cp rendered.toml $out
            '';

          # The M0 demo's single-host co-deploy (docs/M0-DEMO.md): capmeshd + nmidid on the
          # SuperCollider host, wired so the auto-mount headline path works and both §9 trust
          # gates (file-perm + peer-cred) admit the co-located capmeshd. Gate-checks the runbook.
          checks.m0-demo-codeploy =
            let
              demo = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.capmesh
                  self.nixosModules.nmidid
                  ({ config, ... }: {
                    boot.loader.grub.enable = false;
                    fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
                    system.stateVersion = "24.11";
                    # MIDI data-plane daemon: its 0660 control socket is group-owned by the shared
                    # capmesh group (file-perm gate) and it admits peers in that group (peer-cred).
                    services.nmidid = {
                      enable = true;
                      socket = "/run/nmidid/nmidid.sock";
                      socketGroup = config.services.capmesh.group;
                      allowedGroups = [ config.services.capmesh.group ];
                    };
                    # Control plane: advertise the local MIDI capability and auto-mount a remote
                    # MIDI source as a local virtual source (the M0 headline path).
                    services.capmesh = {
                      enable = true;
                      hostId = "sc-host";
                      advertise.midi.enable = true;
                      advertise.midi.socket = config.services.nmidid.socket;
                      automount = [{
                        match = { kind = "midi"; dir = "source"; };
                        action = "mirror-local";
                        lifetime = "while-advertised";
                      }];
                    };
                  })
                ];
              };
              nmididExec = toString (demo.config.systemd.services.nmidid.serviceConfig.ExecStart or "");
              nmididGroup = toString (demo.config.systemd.services.nmidid.serviceConfig.Group or "");
              capmeshSup = toString (demo.config.systemd.services.capmesh.serviceConfig.SupplementaryGroups or [ ]);
            in
            pkgs.runCommand "capmesh-m0-demo-codeploy"
              {
                inherit nmididExec nmididGroup capmeshSup;
              } ''
              cp ${demo.config.environment.etc."capmesh/capmesh.toml".source} rendered.toml
              cat rendered.toml
              # The auto-mount headline path is configured on the SC host.
              grep -q '\[\[automount\]\]' rendered.toml
              grep -q 'action = "mirror-local"' rendered.toml
              # §9 co-deploy: both gates point at the one shared group, so the co-located capmeshd
              # (SupplementaryGroups=capmesh) can open nmidid's 0660 socket AND pass its peer-cred check.
              printf 'nmidid Group=%s capmesh sup=%s\n  nmidid ExecStart=%s\n' \
                "$nmididGroup" "$capmeshSup" "$nmididExec"
              [ "$nmididGroup" = "capmesh" ]
              [ "$capmeshSup" = "capmesh" ]
              echo "$nmididExec" | grep -q -- '--allow-group capmesh'
              cp rendered.toml $out
            '';

          devShells.default = pkgs.mkShell {
            packages = [ pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt pkgs.rust-analyzer ];
          };
        });
    in
    perSystem // {
      nixosModules.default = import ./nix/module.nix { inherit self; };
      nixosModules.capmesh = self.nixosModules.default;
      nixosModules.nmidid = import ./nix/nmidid-module.nix { inherit self; };
      nixosModules.surfaced = import ./nix/surfaced-module.nix { inherit self; };
    };
}
