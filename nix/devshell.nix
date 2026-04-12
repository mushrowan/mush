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
    cargo-nextest
    cargo-watch
    gh
    jujutsu
    onnxruntime
    openssl
    pkg-config
    samply
  ];

  env = {
    ORT_LIB_LOCATION = "${pkgs.onnxruntime}/lib";
    ORT_PREFER_DYNAMIC_LINK = "1";
  };
}
