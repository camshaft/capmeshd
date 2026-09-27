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
            cargoTestFlags = [ "-p" "capmeshd" ];
            # doCheck (default true) runs `cargo test -p capmeshd` in the sandbox — the
            # discovery TXT-schema and config-parser unit tests are pure, no network.
            meta = {
              description = "LAN capability mesh: stateless control-plane daemon (MIDI-first)";
              license = pkgs.lib.licenses.mit;
              mainProgram = "capmeshd";
            };
          };
        in
        {
          packages.default = capmeshd;
          packages.capmeshd = capmeshd;

          checks.clippy = capmeshd.overrideAttrs (old: {
            pname = "${old.pname}-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy -p capmeshd --all-targets --release -- -D warnings";
            installPhase = "touch $out";
            doCheck = false;
          });

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
                    };
                  })
                ];
              };
            in
            pkgs.runCommand "capmesh-module-render" { } ''
              cp ${sys.config.environment.etc."capmesh/capmesh.toml".source} rendered.toml
              cat rendered.toml
              grep -q 'host-id = "check-host"' rendered.toml
              grep -q 'protocol = "nmidi-ctl"' rendered.toml
              grep -q 'socket = "/run/nmidid.sock"' rendered.toml
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
    };
}
