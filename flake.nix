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
      # The test device's firmware target (docs/test-device.md "Building"),
      # which mise.lock's Rust entry must also install.
      firmwareTarget = "thumbv8m.main-none-eabihf";
      firmwareToolchain =
        assert pkgs.lib.assertMsg (builtins.elem firmwareTarget (
          pkgs.lib.splitString "," (rust.options.targets or "")
        )) "mise.lock's Rust targets must include ${firmwareTarget}";
        pkgs.rust-bin.stable.${rust.version}.minimal.override {
          extensions = [
            "rustfmt"
            "clippy"
          ];
          targets = [ firmwareTarget ];
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
      # The FFmpeg build's target (docs/setup.md "FFmpeg"): Windows on x86-64
      # with the Universal C runtime, by this nixpkgs' cross toolchain.
      cross = pkgs.pkgsCross.ucrt64;
      # zlib 1.4, which fixes inflate (ffmpeg.toml's [zlib]), over this
      # nixpkgs' 1.3.2: the release tarball checked by its sha256, and none of
      # 1.3.2's patches, since 1.4 carries the one it has (export-variable.patch,
      # madler/zlib#1171). Drop the override once the pinned nixpkgs has 1.4.
      zlib = cross.zlib.overrideAttrs {
        version = "1.4";
        src = pkgs.fetchurl {
          urls = [
            "https://github.com/madler/zlib/releases/download/v1.4/zlib-1.4.tar.gz"
            "https://www.zlib.net/fossils/zlib-1.4.tar.gz"
          ];
          hash = "sha256-At0Vl4NnenScBK290CtVuKK8x+BU9Rt2tUkZmpFq1YQ=";
        };
        patches = [ ];
      };
      # What FFmpeg's configure and make run with: the only directories on
      # their PATH (`cargo xtask ffmpeg build` clears the rest of the
      # environment). gcc (native) is configure's host compiler.
      ffmpegTools = [
        cross.buildPackages.gcc
        cross.buildPackages.binutils
        pkgs.gcc
        pkgs.nasm
        pkgs.gnumake
        pkgs.pkg-config
        pkgs.bash
        pkgs.coreutils
        pkgs.findutils
        pkgs.diffutils
        pkgs.gnused
        pkgs.gnugrep
        pkgs.gawk
      ];
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
        # The FFmpeg build (docs/setup.md "FFmpeg"): `cargo xtask ffmpeg
        # build` with the cross toolchain, mcfgthread's static archive (the
        # toolchain's thread library, linked statically so no DLL of it is
        # imported) and zlib; nixpkgs' FFmpeg, with x264, and mkvtoolnix make
        # the committed media fixtures (`cargo xtask media fixtures`).
        ffmpeg = pkgs.mkShell {
          packages = [
            toolchain
            pkgs.git
            pkgs.curl
            pkgs.gnutar
            pkgs.gzip
            pkgs.xz
            pkgs.zip
            pkgs.unzip
            pkgs.ffmpeg
            pkgs.mkvtoolnix-cli
          ];
          shellHook = rootHash + ''
            export CARGO_TARGET_DIR=${cacheDir "ffmpeg-target"}
            export TAPESTRY_FFMPEG_PATH=${pkgs.lib.makeBinPath ffmpegTools}
            export TAPESTRY_FFMPEG_MCFGTHREAD=${cross.windows.mcfgthreads}/lib/libmcfgthread.a
            export TAPESTRY_FFMPEG_ZLIB=${zlib.out}
            export TAPESTRY_FFMPEG_ZLIB_DEV=${zlib.dev}
            export TAPESTRY_FFMPEG_ZLIB_SRC=${zlib.src}
            export TAPESTRY_FFMPEG_ZLIB_VERSION=${zlib.version}
            export TAPESTRY_NIXPKGS_REV=${nixpkgs.rev}
          '';
        };
        # The test device's firmware (docs/test-device.md "Building"): the
        # toolchain with the RP2350 target, and picotool to check its UF2.
        test-device = pkgs.mkShell {
          packages = [
            firmwareToolchain
            pkgs.picotool
            pkgs.git
          ];
          shellHook = rootHash + ''
            export CARGO_TARGET_DIR=${cacheDir "test-device-target"}
          '';
        };
      };
    };
}
