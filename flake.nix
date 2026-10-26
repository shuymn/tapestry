{
  description = "tapestry Linux dev shell (WSL/CI)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      # Single source of truth: the Rust version locked by mise.
      lock = builtins.fromTOML (builtins.readFile ./mise.lock);
      rustEntries = lock.tools.rust or [ ];
      rust =
        assert pkgs.lib.assertMsg (
          builtins.length rustEntries == 1 && (builtins.head rustEntries).backend or null == "core:rust"
        ) "mise.lock must contain exactly one tools.rust entry with backend \"core:rust\"";
        builtins.head rustEntries;
      toolchain = pkgs.rust-bin.stable.${rust.version}.minimal.override {
        extensions = [
          "rustfmt"
          "clippy"
        ];
      };
      # The nightly of the `fuzz` shell (cargo-fuzz needs nightly for the
      # sanitizers). Bumped by hand, for example when stable passes it; the
      # date must exist in the locked rust-overlay.
      fuzzNightly = "2026-10-23";
      nightly = pkgs.rust-bin.nightly.${fuzzNightly}.minimal;
      # One directory per working copy (hash of its root), inside the Linux
      # filesystem, so working copies never share output and nothing is
      # written over /mnt/c.
      cacheDir = name: ''"''${XDG_CACHE_HOME:-$HOME/.cache}/tapestry/${name}/$root_hash"'';
      rootHash = ''
        root_hash=$(printf %s "$PWD" | sha256sum | cut -c1-16)
      '';
    in
    {
      devShells.${system} = {
        default = pkgs.mkShell {
          packages = [
            toolchain
            pkgs.git
          ];
          shellHook = rootHash + ''
            export CARGO_TARGET_DIR=${cacheDir "target"}
          '';
        };
        # Fuzzing (docs/setup.md "Fuzzing"): the dated nightly and cargo-fuzz.
        # A separate shell, since stable and nightly cannot share PATH.
        fuzz = pkgs.mkShell {
          packages = [
            nightly
            pkgs.cargo-fuzz
            pkgs.git
          ];
          shellHook = rootHash + ''
            export CARGO_TARGET_DIR=${cacheDir "fuzz-target"}
            export TAPESTRY_FUZZ_CORPUS=${cacheDir "fuzz-corpus"}
          '';
        };
      };
    };
}
