{
  craneLib,
  src,
  ripgrep,
  fd,
}: let
  commonArgs = {
    inherit src;
    pname = "mush";
    version = "0.1.0";
    strictDeps = true;
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in {
  inherit cargoArtifacts;

  package = craneLib.buildPackage (commonArgs
    // {
      inherit cargoArtifacts;
      nativeBuildInputs = [ripgrep fd];
    });

  clippy = craneLib.cargoClippy (commonArgs
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets -- --deny warnings";
    });

  test = craneLib.cargoNextest (commonArgs
    // {
      inherit cargoArtifacts;
      nativeBuildInputs = [ripgrep fd];
    });

  fmt = craneLib.cargoFmt {
    inherit src;
    pname = "mush";
  };
}
