{
  pkgs,
  craneLib,
  src,
  fd,
  openssl,
  pkg-config,
  cacert,
  enableProfiling ? false,
  enableSignal ? false,
}: let
  featureFlags =
    pkgs.lib.optionals enableProfiling ["mush-cli/profiling"]
    ++ pkgs.lib.optionals enableSignal ["mush-cli/signal"];

  cargoExtraArgs =
    if featureFlags == []
    then ""
    else "--features " + builtins.concatStringsSep "," featureFlags;

  commonArgs =
    {
      inherit src cargoExtraArgs;
      pname = "mush";
      version = "0.1.0";
      strictDeps = true;
      meta.mainProgram = "mush";

      nativeBuildInputs = [pkg-config];
      buildInputs = [openssl];
    }
    // (
      if enableProfiling
      then {
        CARGO_PROFILE = "profiling";
        RUSTFLAGS = "-C force-frame-pointers=yes";
      }
      else {}
    );

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  kernelPython = pkgs.python3.withPackages (ps: [
    ps.requests
    ps.httpx
    ps.pyyaml
    ps.tomli
    ps.python-dotenv
    ps.pandas
    ps.numpy
    ps.scipy
    ps.beautifulsoup4
    ps.lxml
    ps.pydantic
    ps.tyro
  ]);

  schemaEmitter = craneLib.buildPackage (commonArgs
    // {
      inherit cargoArtifacts;
      pname = "mush-config-schema";
      cargoExtraArgs = commonArgs.cargoExtraArgs + " --bin mush-config-schema";
      doCheck = false;
    });
in {
  inherit cargoArtifacts;

  package = craneLib.buildPackage (commonArgs
    // {
      inherit cargoArtifacts;
      doCheck = false;
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [fd] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [pkgs.makeWrapper];
      postFixup = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
        wrapProgram $out/bin/mush \
          --prefix PATH : ${pkgs.lib.makeBinPath [pkgs.libnotify]} \
          --set-default MUSH_KERNEL_PYTHON ${kernelPython}/bin/python3
      '';
      SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
    });

  clippy = craneLib.cargoClippy (commonArgs
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets -- --deny warnings";
    });

  test = craneLib.cargoNextest (commonArgs
    // {
      inherit cargoArtifacts;
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [fd pkgs.jujutsu];
      SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
    });

  fmt = craneLib.cargoFmt {
    inherit src;
    pname = "mush";
  };

  deny = craneLib.cargoDeny (commonArgs
    // {
      inherit cargoArtifacts;
    });

  doctest = craneLib.cargoDocTest (commonArgs
    // {
      inherit cargoArtifacts;
    });

  schemaCheck = pkgs.runCommand "mush-config-schema-check" {} ''
    ${pkgs.diffutils}/bin/diff -u ${../nix/config-schema.json} \
      <(${schemaEmitter}/bin/mush-config-schema)
    touch $out
  '';

  shear = craneLib.mkCargoDerivation (commonArgs
    // {
      cargoArtifacts = null;
      pnameSuffix = "-shear";
      doInstallCargoArtifacts = false;
      buildPhaseCargoCommand = "cargo-shear";
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [pkgs.cargo-shear];
    });
}
