{
  pkgs,
  craneLib,
  checks ? {},
  cargoArtifacts ? null,
}:
craneLib.devShell {
  inherit checks cargoArtifacts;

  packages = with pkgs; [
    cargo-edit
    cargo-machete
    cargo-watch
    cargo-nextest
    jj
    gh
    pkg-config
    openssl
  ];

  env = {
    RUST_LOG = "info";
  };
}
