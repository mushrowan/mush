{
  description = "mush - a minimal, extensible coding agent harness in rust";

  inputs = {
    nixpkgs.url = "git+https://github.com/nixos/nixpkgs?shallow=1&ref=nixos-unstable";

    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-parts = {
      url = "git+https://github.com/hercules-ci/flake-parts?shallow=1";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    git-hooks = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {flake-parts, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];

      flake.homeManagerModules = let
        mod = import ./nix/module.nix inputs.self;
      in {
        mush = mod;
        default = mod;
      };

      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks.flakeModule
      ];

      perSystem = {
        system,
        self',
        config,
        ...
      }: let
        pkgs = import inputs.nixpkgs {
          inherit system;
          overlays = [(import inputs.rust-overlay)];
        };

        rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = ["rust-src" "rust-analyzer"];
        };

        craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

        src = craneLib.cleanCargoSource ./.;

        craneOutputs = import ./nix/package.nix {
          inherit craneLib src;
          inherit (pkgs) ripgrep fd onnxruntime pkg-config openssl cacert;
        };

        craneOutputsWithEmbeddings = import ./nix/package.nix {
          inherit craneLib src;
          inherit (pkgs) ripgrep fd onnxruntime pkg-config openssl cacert;
          enableEmbeddings = true;
        };

        craneOutputsProfiling = import ./nix/package.nix {
          inherit craneLib src;
          inherit (pkgs) ripgrep fd onnxruntime pkg-config openssl cacert;
          enableProfiling = true;
        };
      in {
        packages.default = craneOutputs.package;
        packages.with-embeddings = craneOutputsWithEmbeddings.package;
        packages.debug = pkgs.writeShellScriptBin "mush-debug" ''
          export RUST_LOG=''${RUST_LOG:-warn,mush_agent=debug,mush_ai=debug,mush_tools=debug,mush_tui=debug,mush_cli=debug,mush_ext=debug,mush_session=debug,mush_lsp=debug,mush_mcp=debug,mush_treesitter=debug}
          echo "logging to ''${XDG_DATA_HOME:-$HOME/.local/share}/mush/mush.log" >&2
          echo "set RUST_LOG=...,mush_agent=trace,mush_ai=trace for full request/response bodies" >&2
          exec ${craneOutputs.package}/bin/mush "$@"
        '';
        packages.profiler = pkgs.writeShellScriptBin "mush-profiler" ''
          export RUST_LOG=''${RUST_LOG:-warn,mush_agent=info,mush_ai=info,mush_tools=info,mush_tui=info,mush_cli=info,mush_ext=info,mush_mcp=info,mush_treesitter=info}
          export MUSH_TRACE=true
          export MUSH_PROFILE_STARTUP=true
          echo "profiling: tracing timeline + startup phases enabled" >&2
          echo "trace output: ''${XDG_DATA_HOME:-$HOME/.local/share}/mush/mush-trace-$$.json" >&2
          echo "open traces at https://ui.perfetto.dev" >&2
          echo "" >&2
          exec ${craneOutputsProfiling.package}/bin/mush "$@"
        '';
        packages.samply = pkgs.writeShellScriptBin "mush-samply" ''
          echo "sampling profiler: cpu flamegraph via samply" >&2
          echo "firefox profiler will open automatically" >&2
          echo "" >&2
          exec ${pkgs.samply}/bin/samply record ${craneOutputsProfiling.package}/bin/mush "$@"
        '';

        checks = {
          inherit (craneOutputs) package clippy test fmt deny doctest;
        };

        devShells.default = import ./nix/devshell.nix {
          inherit pkgs craneLib;
          inherit (craneOutputs) cargoArtifacts;
          inherit (self') checks;
          shellHook = config.pre-commit.installationScript;
        };

        pre-commit.settings.hooks = {
          treefmt.enable = true;
          treefmt.package = config.treefmt.build.wrapper;
        };

        treefmt = {
          projectRootFile = "flake.nix";
          programs = {
            alejandra.enable = true;
            deadnix.enable = true;
            statix.enable = true;
            rustfmt.enable = true;
            taplo.enable = true;
          };
        };
      };
    };
}
