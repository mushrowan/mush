let
  optionals = condition: values:
    if condition
    then values
    else [];
  pkgs = {
    lib = {
      inherit optionals;
      optionalString = condition: value:
        if condition
        then value
        else "";
      makeBinPath = _: "/desktop/bin";
    };
    stdenv.isLinux = true;
    makeWrapper = "wrapper";
    libnotify = "desktop";
  };
  build = flags:
    import ./package.nix ({
        inherit pkgs;
        craneLib = {
          buildDepsOnly = args: args;
          buildPackage = args: args;
        };
        src = "source";
        fd = "fd";
        pkg-config = "pkg-config";
        openssl = "openssl";
        cacert = "cacert";
      }
      // flags);
  desktop = build {};
  signal = build {enableSignal = true;};
  both = build {
    enableSignal = true;
    enableProfiling = true;
  };
in
  assert desktop.package.cargoExtraArgs == "";
  assert signal.package.cargoExtraArgs == "--features mush-cli/signal";
  assert signal.cargoArtifacts.cargoExtraArgs == signal.package.cargoExtraArgs;
  assert both.package.cargoExtraArgs == "--features mush-cli/profiling,mush-cli/signal";
  assert signal.package.buildInputs == desktop.package.buildInputs;
  assert signal.package.nativeBuildInputs == desktop.package.nativeBuildInputs;
  assert signal.package.postFixup == desktop.package.postFixup; {
    desktop = desktop.package.cargoExtraArgs;
    signal = signal.package.cargoExtraArgs;
    combined = both.package.cargoExtraArgs;
    runtimeDependenciesUnchanged = true;
  }
