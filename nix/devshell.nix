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
    cargo-deny
    jujutsu
    gh
    pkg-config
    openssl
    onnxruntime
  ];

  env = {
    ORT_LIB_LOCATION = "${pkgs.onnxruntime}/lib";
    ORT_PREFER_DYNAMIC_LINK = "1";
  };
}
