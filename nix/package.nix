{
  craneLib,
  src,
  ripgrep,
  fd,
  onnxruntime,
  openssl,
  pkg-config,
}: let
  commonArgs = {
    inherit src;
    pname = "mush";
    version = "0.1.0";
    strictDeps = true;

    nativeBuildInputs = [pkg-config];
    buildInputs = [onnxruntime openssl];

    # point ort at nix-provided onnxruntime instead of downloading
    env.ORT_LIB_LOCATION = "${onnxruntime}/lib";
    env.ORT_PREFER_DYNAMIC_LINK = "1";
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in {
  inherit cargoArtifacts;

  package = craneLib.buildPackage (commonArgs
    // {
      inherit cargoArtifacts;
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ripgrep fd];
    });

  clippy = craneLib.cargoClippy (commonArgs
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets -- --deny warnings";
    });

  test = craneLib.cargoNextest (commonArgs
    // {
      inherit cargoArtifacts;
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ripgrep fd];
    });

  fmt = craneLib.cargoFmt {
    inherit src;
    pname = "mush";
  };
}
