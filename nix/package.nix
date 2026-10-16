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
    ps.dill
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

  package = craneLib.buildPackage (commonArgs
    // {
      inherit cargoArtifacts;
      doCheck = false;
      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [fd pkgs.makeWrapper pkgs.installShellFiles];
      postInstall = pkgs.lib.optionalString (pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform) ''
        installShellCompletion --cmd mush \
          --bash <($out/bin/mush completions bash) \
          --fish <($out/bin/mush completions fish) \
          --nushell <($out/bin/mush completions nushell) \
          --zsh <($out/bin/mush completions zsh)
      '';
      postFixup = ''
        wrapProgram $out/bin/mush \
          ${pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux "--prefix PATH : ${pkgs.lib.makeBinPath [pkgs.libnotify]}"} \
          --set-default MUSH_KERNEL_PYTHON ${kernelPython}/bin/python3
      '';
      SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";
    });
in {
  inherit cargoArtifacts package;

  completionsCheck = pkgs.runCommand "mush-completions-check" {} ''
    share=${package}/share
    export HOME=$TMPDIR
    mkdir bin
    printf '#!${pkgs.runtimeShell}\n' > bin/mush
    chmod +x bin/mush
    export PATH=$PWD/bin:$PATH

    XDG_DATA_DIRS=$share ${pkgs.lib.getExe pkgs.bashInteractive} --norc --noprofile -c '
      source ${pkgs.bash-completion}/share/bash-completion/bash_completion
      _comp_load mush
      COMP_WORDS=(mush comp) COMP_CWORD=1
      _mush mush comp mush
      [[ "''${COMPREPLY[*]}" == completions ]]
    '
    ${pkgs.lib.getExe pkgs.zsh} -f -c "
      fpath=($share/zsh/site-functions \$fpath)
      autoload -U compinit && compinit -u -D
      [[ \$_comps[mush] == _mush ]]
    "
    ${pkgs.lib.getExe pkgs.fish} --no-config -c "
      set -g fish_complete_path $share/fish/vendor_completions.d
      complete -C 'mush comp' | string match -q 'completions*'
    "
    ${pkgs.lib.getExe pkgs.nushell} --no-config-file -c "
      source $share/nushell/vendor/autoload/mush.nu
      mush --resume
      mush --resume abc
    "
    touch $out
  '';

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
