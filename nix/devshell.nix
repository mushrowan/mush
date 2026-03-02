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
    jujutsu
    gh
    pkg-config
    openssl
    onnxruntime
  ];

  env = {
    RUST_LOG = "info";
    ORT_LIB_LOCATION = "${pkgs.onnxruntime}/lib";
    ORT_PREFER_DYNAMIC_LINK = "1";
  };
}
