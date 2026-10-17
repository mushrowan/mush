{
  pkgs,
  craneLib,
  checks ? {},
  cargoArtifacts ? null,
  shellHook ? "",
}:
craneLib.devShell {
  inherit checks cargoArtifacts shellHook;

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
