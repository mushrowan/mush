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

    nixcfg = {
      url = "github:mushrowan/nixcfg";
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
          extensions = ["rust-src" "rust-analyzer" "llvm-tools-preview"];
        };

        craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

        src = craneLib.cleanCargoSource ./.;

        craneOutputs = import ./nix/package.nix {
          inherit craneLib pkgs src;
          inherit (pkgs) fd onnxruntime pkg-config openssl cacert;
        };

        craneOutputsProfiling = import ./nix/package.nix {
          inherit craneLib pkgs src;
          inherit (pkgs) fd onnxruntime pkg-config openssl cacert;
          enableProfiling = true;
        };
      in {
        packages.default = craneOutputs.package;
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
          data_dir="''${XDG_DATA_HOME:-$HOME/.local/share}/mush/profiles"
          mkdir -p "$data_dir"
          out="$data_dir/mush-$(date +%Y%m%d-%H%M%S).json.gz"

          # samply's child-exit path can leave the terminal in a weird
          # state: raw mode still on, mouse tracking still reporting,
          # alt-screen not restored, or escape codes echoed literally.
          # this happens whether mush exits cleanly, panics, or gets
          # killed by samply on ctrl+c.
          #
          # guarantee cleanup on every exit path (normal, error, signal)
          # via an EXIT trap so the user's shell is always usable afterwards
          restore_terminal() {
            # disable mouse tracking modes that mush enabled
            printf '\e[?1002l\e[?1003l\e[?1006l' >&2 2>/dev/null || true
            # pop keyboard enhancement flags (kitty protocol)
            printf '\e[<u' >&2 2>/dev/null || true
            # disable bracketed paste and focus change reporting
            printf '\e[?2004l\e[?1004l' >&2 2>/dev/null || true
            # leave alt screen, show cursor, reset styling
            printf '\e[?1049l\e[?25h\e[0m' >&2 2>/dev/null || true
            # restore default cursor shape
            printf '\e[0 q' >&2 2>/dev/null || true
            # drop raw mode + echo as a belt-and-braces for any leftover
            # tty state (harmless no-op if stdin isn't a tty)
            stty sane 2>/dev/null || true
          }
          trap restore_terminal EXIT INT TERM

          echo "sampling profiler: cpu flamegraph via samply" >&2
          echo "recording to: $out" >&2
          echo "" >&2
          ${pkgs.samply}/bin/samply record --save-only -o "$out" \
            ${craneOutputsProfiling.package}/bin/mush "$@"
          echo "" >&2
          echo "profile saved: $out" >&2
          echo "open it with: nix run .#samply-load -- $out" >&2
          echo "or manually:  samply load $out" >&2
        '';
        packages.samply-load = pkgs.writeShellScriptBin "mush-samply-load" ''
          data_dir="''${XDG_DATA_HOME:-$HOME/.local/share}/mush/profiles"
          if [ -n "$1" ]; then
            target="$1"
          else
            target=$(ls -t "$data_dir"/mush-*.json.gz 2>/dev/null | head -n 1)
            if [ -z "$target" ]; then
              echo "no saved profiles in $data_dir" >&2
              echo "record one first with: nix run .#samply" >&2
              exit 1
            fi
            echo "loading latest: $target" >&2
          fi
          exec ${pkgs.samply}/bin/samply load "$target"
        '';

        checks = {
          inherit (craneOutputs) package clippy test fmt deny doctest schemaCheck;
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
          # rust formatting stays with cargo (crane runs `cargo fmt --check`
          # against the pinned nightly toolchain). treefmt covers everything
          # else so the two formatters never disagree on style edition.
          programs = {
            alejandra.enable = true;
            deadnix.enable = true;
            statix.enable = true;
            taplo.enable = true;
          };
        };
      };
    };
}
