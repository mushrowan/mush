{
  pkgs,
  craneLib,
  src,
  fd,
  openssl,
  pkg-config,
  cacert,
  enableProfiling ? false,
}: let
  featureFlags =
    if enableProfiling
    then ["profiling"]
    else [];

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

      nativeBuildInputs = [pkg-config];
      buildInputs = [openssl];
    }
    // (
      if enableProfiling
      then {
        # use the profiling cargo profile (release + debug symbols)
        CARGO_PROFILE = "profiling";
        # frame pointers let the profiler (samply/perf) unwind stacks
        # through our own code. precompiled std and libc still lack them,
        # so call graphs degrade once a stack crosses into them, but paths
        # that stay within mush + its source-built deps unwind correctly.
        RUSTFLAGS = "-C force-frame-pointers=yes";
      }
      else {}
    );

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  # emit the config schema from rust and diff it against the checked-in
  # nix/config-schema.json. catches drift when any field in `Config` or
  # its nested types changes without regenerating the schema.
  #
  # regenerate with:
  #   cargo run -p mush-cli --bin mush-config-schema > nix/config-schema.json
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
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [fd];
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
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [fd];
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
}
