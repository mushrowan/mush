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
        ./nix/hydra.nix
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

        rustToolchainFor = p:
          p.rust-bin.selectLatestNightlyWith (toolchain:
            toolchain.default.override {
              extensions = ["rust-src" "rust-analyzer" "llvm-tools-preview" "rustfmt"];
            });

        craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchainFor;

        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          name = "source";
          filter = path: type:
            (builtins.match ".*tests/fixtures/.*" path != null)
            || (builtins.match ".*/snapshots/.*" path != null)
            || (builtins.match ".*/mush-kernel/shim/.*" path != null)
            || (builtins.match ".*/mush-cli/skills(/.*)?" path != null)
            || (craneLib.filterCargoSources path type);
        };

        craneOutputs = import ./nix/package.nix {
          inherit craneLib pkgs src;
          inherit (pkgs) fd pkg-config openssl cacert;
        };

        craneOutputsProfiling = import ./nix/package.nix {
          inherit craneLib pkgs src;
          inherit (pkgs) fd pkg-config openssl cacert;
          enableProfiling = true;
        };
        craneOutputsSignal = import ./nix/package.nix {
          inherit craneLib pkgs src;
          inherit (pkgs) fd pkg-config openssl cacert;
          enableSignal = true;
        };
      in {
        packages.default = craneOutputs.package;
        packages.signal = craneOutputsSignal.package;
        apps.supervisor = {
          type = "app";
          program = "${craneOutputs.package}/bin/mush-supervisor";
        };
        packages.debug = pkgs.writeShellScriptBin "mush-debug" ''
          export RUST_LOG=''${RUST_LOG:-warn,mush_agent=debug,mush_ai=debug,mush_tools=debug,mush_tui=debug,mush_cli=debug,mush_ext=debug,mush_session=debug,mush_lsp=debug,mush_mcp=debug,mush_treesitter=debug}
          echo "logging to ''${XDG_DATA_HOME:-$HOME/.local/share}/mush/mush.log" >&2
          echo "set RUST_LOG=...,mush_agent=trace,mush_ai=trace for full request/response bodies" >&2
          exec ${craneOutputs.package}/bin/mush "$@"
        '';
        packages.profiler = pkgs.writeShellScriptBin "mush-profiler" ''
          export RUST_LOG=''${RUST_LOG:-warn,mush_agent=info,mush_ai=info,mush_tools=info,mush_tui=info,mush_cli=info,mush_ext=info,mush_mcp=info,mush_treesitter=info,mush_tui::resize=info,mush_tui::scroll=debug}
          export MUSH_TRACE=true
          export MUSH_PROFILE_STARTUP=true
          export MUSH_WIRE_LOG=''${MUSH_WIRE_LOG:-''${XDG_DATA_HOME:-$HOME/.local/share}/mush/wire.jsonl}
          echo "profiling: tracing timeline + startup phases enabled" >&2
          echo "wire log: $MUSH_WIRE_LOG (full request/response/sse journal)" >&2
          echo "resize/scroll diagnostics: mush_tui::resize=info, mush_tui::scroll=debug" >&2
          echo "trace output: ''${XDG_DATA_HOME:-$HOME/.local/share}/mush/mush-trace-$$.json" >&2
          echo "open traces at https://ui.perfetto.dev" >&2
          echo "" >&2
          exec ${craneOutputsProfiling.package}/bin/mush "$@"
        '';
        packages.perf = pkgs.writeShellScriptBin "mush-perf" ''
          data_dir="''${XDG_DATA_HOME:-$HOME/.local/share}/mush/profiles/perf-$(date +%Y%m%d-%H%M%S)"
          mkdir -p "$data_dir"

          export RUST_LOG=''${RUST_LOG:-warn,mush_agent=debug,mush_ai=debug,mush_ai::providers::sse::wire=off,mush_tools=debug,mush_tui=debug,mush_cli=debug,mush_ext=debug,mush_session=debug,mush_lsp=debug,mush_mcp=debug,mush_treesitter=debug,mush_tui::draw_reason=trace,mush_tui::scroll=trace,mush_tui::resize=trace}
          export MUSH_TRACE=true
          export MUSH_PROFILE_STARTUP=true
          export MUSH_TRACE_DIR="$data_dir"
          export MUSH_WIRE_LOG="$data_dir/wire.jsonl"
          export MUSH_STALL_WATCHDOG=true
          export MUSH_STALL_DIR="$data_dir/stall-dumps"

          profile="$data_dir/profile.json.gz"
          samply_stderr="$data_dir/samply.stderr.log"
          printf '%s\n' "${craneOutputsProfiling.package}/bin/mush $*" > "$data_dir/command.txt"

          # a TUI sampled under a profiler can be left with raw mode on,
          # mouse tracking still reporting, or the alt screen not restored,
          # on any exit path (clean, panic, ctrl+c). restore the terminal
          # unconditionally so the shell stays usable afterwards.
          restore_terminal() {
            printf '\e[?1002l\e[?1003l\e[?1006l' >&2 2>/dev/null || true
            printf '\e[<u' >&2 2>/dev/null || true
            printf '\e[?2004l\e[?1004l' >&2 2>/dev/null || true
            printf '\e[?1049l\e[?25h\e[0m' >&2 2>/dev/null || true
            printf '\e[0 q' >&2 2>/dev/null || true
            stty sane 2>/dev/null || true
          }
          trap restore_terminal EXIT INT TERM

          echo "perf profiling: samply cpu profile + tracing timeline + startup phases + logs" >&2
          echo "resize/scroll diagnostics: mush_tui::resize=trace, mush_tui::scroll=trace" >&2
          echo "output dir: $data_dir" >&2
          echo "log file: ''${XDG_DATA_HOME:-$HOME/.local/share}/mush/mush.log" >&2
          echo "wire log: $MUSH_WIRE_LOG (full request/response/sse journal)" >&2
          echo "trace dir: $MUSH_TRACE_DIR" >&2
          echo "stall dumps: $MUSH_STALL_DIR" >&2
          echo "" >&2

          set +e
          ${pkgs.samply}/bin/samply record --save-only \
            --rate ''${MUSH_SAMPLY_RATE:-999} \
            -o "$profile" \
            ${craneOutputsProfiling.package}/bin/mush "$@" 2> "$samply_stderr"
          status=$?
          set -e

          MUSH_ADDR2LINE=${pkgs.binutils}/bin/addr2line \
            ${pkgs.lib.getExe pkgs.python3} ${./nix/perf_summary.py} "$data_dir" > "$data_dir/perf.summary.txt" 2> "$data_dir/perf.summary.stderr" || true

          echo "" >&2
          echo "perf bundle saved: $data_dir" >&2
          echo "samply profile: $profile" >&2
          echo "summary: $data_dir/perf.summary.txt (stalls + trace timings + cpu self-time)" >&2
          echo "open the profile with: nix run .#perf-load" >&2
          echo "             or:       samply load $profile" >&2

          # the framehop unwinder noise sits in the captured stderr. surface
          # only the non-noise lines (a panic or eyre report) so they aren't
          # lost behind the redirect, and point at the full file
          if [ -s "$samply_stderr" ]; then
            grep -vE 'two modules at the same start address|Lost [0-9]+ events' "$samply_stderr" >&2 || true
            echo "samply stderr (incl. unwinder noise): $samply_stderr" >&2
          fi
          exit "$status"
        '';
        packages.perf-load = pkgs.writeShellScriptBin "mush-perf-load" ''
          base="''${XDG_DATA_HOME:-$HOME/.local/share}/mush/profiles"
          if [ -n "$1" ]; then
            target="$1"
          else
            target=$(ls -t "$base"/perf-*/profile.json.gz 2>/dev/null | head -n 1)
            if [ -z "$target" ]; then
              echo "no perf bundles with a samply profile in $base" >&2
              echo "record one first with: nix run .#perf" >&2
              exit 1
            fi
            echo "loading latest: $target" >&2
          fi
          exec ${pkgs.samply}/bin/samply load "$target"
        '';

        checks = {
          inherit (craneOutputs) package clippy test fmt deny doctest schemaCheck shear;
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
