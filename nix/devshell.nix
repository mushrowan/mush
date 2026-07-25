{
  pkgs,
  craneLib,
  checks ? {},
  cargoArtifacts ? null,
  shellHook ? "",
}:
craneLib.devShell {
  inherit checks cargoArtifacts;

  shellHook = ''
    export CARGO_HOME="$PWD/.direnv/cargo-home"
    mkdir -p "$CARGO_HOME"
    ${shellHook}
  '';

  packages = with pkgs; [
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
  ];
}
