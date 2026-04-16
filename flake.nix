{
  description = "WebRTC custom transport for iroh";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay, ... }:
  let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
  in {
    packages = forAllSystems (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        craneLib = crane.mkLib pkgs;

        src = craneLib.cleanCargoSource (craneLib.path ./.);

        commonArgs = {
          pname = "iroh-webrtc";
          version = "0.1.0";
          inherit src;
          strictDeps = true;
          buildInputs = with pkgs; [ openssl ] ++
            pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.darwin.apple_sdk.frameworks.Security
            ];
          nativeBuildInputs = with pkgs; [ pkg-config clang ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in {
        default = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          doCheck = true;
        });

        deps = cargoArtifacts;
      }
    );

    checks = forAllSystems (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        craneLib = crane.mkLib pkgs;
        src = craneLib.cleanCargoSource (craneLib.path ./.);
        commonArgs = {
          pname = "iroh-webrtc";
          version = "0.1.0";
          inherit src;
          strictDeps = true;
          buildInputs = with pkgs; [ openssl ];
          nativeBuildInputs = with pkgs; [ pkg-config clang ];
        };
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      in {
        clippy = craneLib.cargoClippy (commonArgs // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        });

        test = craneLib.cargoNextest (commonArgs // {
          inherit cargoArtifacts;
          cargoNextestExtraArgs = "--no-tests=pass";
        });
      }
    );

    devShells = forAllSystems (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      in {
        default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rust-bin.stable.latest.default
            pkg-config
            openssl
            clang
          ];
        };
      }
    );
  };
}
