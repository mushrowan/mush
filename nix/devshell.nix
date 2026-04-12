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
    samply
  ];

  env = {
    ORT_LIB_LOCATION = "${pkgs.onnxruntime}/lib";
    ORT_PREFER_DYNAMIC_LINK = "1";
  };
}
