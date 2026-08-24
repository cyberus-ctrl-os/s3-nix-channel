{
  description = "Build a cargo project without extra checks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, crane, flake-utils, ... }: {
    herculesCI.ciSystems = [
      "x86_64-linux"
    ];

    nixosModules.default = { config, lib, pkgs, ... }: {
      imports = [
        ./nix/module.nix
      ];

      nixpkgs.overlays = [
        (final: prev: {
          s3-nix-channel = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        })
      ];
    };

  } // flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        craneLib = crane.mkLib pkgs;

        # Common arguments can be set here to avoid repeating them later
        # Note: changes here will rebuild all dependency crates
        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;

          buildInputs = [
            # Add additional build inputs here
          ];
        };

        # Build *just* the cargo dependencies, so we can reuse
        # all of that work (e.g. via cachix) when running in CI
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        s3-nix-channel = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;

          # Additional environment variables or build phases/hooks can be set
          # here *without* rebuilding all dependency crates
          # MY_CUSTOM_VAR = "some value";

          meta = {
            mainProgram = "s3-nix-channel";
          };
        });
      in
      {
        checks = {
          inherit s3-nix-channel;

          # Run clippy (and deny all warnings) on the crate source,
          # again, reusing the dependency artifacts from above.
          #
          # Note that this is done as a separate derivation so that
          # we can block the CI if there are issues here, but not
          # prevent downstream consumers from building our crate by itself.
          s3-nix-channel-clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });
        } // import ./nix/tests.nix { inherit self system; };

        packages.default = s3-nix-channel;

        apps.default = flake-utils.lib.mkApp {
          drv = s3-nix-channel;
        };

        devShells.default = craneLib.devShell {
          # Inherit inputs from checks.
          # checks = self.checks.${system};

          # Additional dev-shell environment variables can be set directly
          # MY_CUSTOM_DEVELOPMENT_VAR = "something else";

          # Extra inputs can be added here; cargo and rustc are provided by default.
          packages = [
            # pkgs.ripgrep
          ];
        };
      });
}
