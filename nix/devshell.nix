{
  pkgs,
  craneLib,
  checks ? {},
  cargoArtifacts ? null,
  shellHook ? "",
}: let
  isX86Linux = pkgs.stdenv.hostPlatform.isLinux && pkgs.stdenv.hostPlatform.isx86_64;
in
  craneLib.devShell {
    inherit checks cargoArtifacts;

    shellHook = ''
      export SCCACHE_DIR="''${SCCACHE_DIR:-$HOME/.cache/sccache}"
      ${shellHook}
    '';

    env =
      {
        RUSTC_WRAPPER = "sccache";
        SCCACHE_CACHE_SIZE = "40G";
      }
      // pkgs.lib.optionalAttrs isX86Linux {
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-C link-arg=-fuse-ld=mold -Zthreads=4";
      };

    packages = with pkgs;
      [
        cargo-deny
        cargo-edit
        cargo-llvm-cov
        cargo-machete
        cargo-shear
        cargo-nextest
        cargo-watch
        gh
        jujutsu
        openssl
        pkg-config
        samply
        sccache
      ]
      ++ lib.optional isX86Linux mold;
  }
