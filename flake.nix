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
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          toolchain
          pkgs.git
        ];
        # One target dir per working copy (hash of its root), inside the Linux
        # filesystem, so working copies never share output and nothing is
        # written over /mnt/c.
        shellHook = ''
          root_hash=$(printf %s "$PWD" | sha256sum | cut -c1-16)
          export CARGO_TARGET_DIR="''${XDG_CACHE_HOME:-$HOME/.cache}/tapestry/target/$root_hash"
        '';
      };
    };
}
