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

        # crane's default filter strips non-rust files. include the
        # `tests/fixtures/` paths so integration tests can `include_str!`
        # vendored JSON fixtures (e.g. the openrouter openapi spec used
        # by the drift test)
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          name = "source";
          filter = path: type:
            (builtins.match ".*tests/fixtures/.*" path != null)
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
      in {
        packages.default = craneOutputs.package;
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
          printf '%s\n' "${craneOutputsProfiling.package}/bin/mush $*" > "$data_dir/command.txt"

          # a TUI sampled under a profiler can be left with raw mode on,
          # mouse tracking still reporting, or the alt screen not restored,
          # on any exit path (clean, panic, ctrl+c). restore the terminal
          # unconditionally so the shell stays usable afterwards. same
          # guard as the samply wrapper.
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

          # samply unwinds online and writes one compact profile on exit, so
          # close stays fast. the old perf path ran perf record with a dwarf
          # call-graph (~4kb of stack per sample) and then perf report/script
          # synchronously on exit, which took minutes on a long session and
          # left a truncated perf.data when killed.
          set +e
          ${pkgs.samply}/bin/samply record --save-only \
            --rate ''${MUSH_SAMPLY_RATE:-999} \
            -o "$profile" \
            ${craneOutputsProfiling.package}/bin/mush "$@"
          status=$?
          set -e

          ${pkgs.lib.getExe pkgs.python3} - "$data_dir" <<'PY' > "$data_dir/perf.summary.txt" 2> "$data_dir/perf.summary.stderr" || true
          import collections
          import json
          import sys
          from pathlib import Path

          data_dir = Path(sys.argv[1])
          trace = next(data_dir.glob("mush-trace-*.json"), None)

          print("mush profile summary")
          print(f"bundle: {data_dir}")
          print()

          stall_dir = data_dir / "stall-dumps"
          stalls = sorted(stall_dir.glob("mush-stall-*.txt")) if stall_dir.exists() else []
          print("tui stall dumps (fire on any await over threshold, incl. idle)")
          if stalls:
              for path in stalls:
                  phase = detail = "unknown"
                  elapsed = "?"
                  for line in path.read_text(errors="replace").splitlines():
                      if line.startswith("phase: "):
                          phase = line[len("phase: "):]
                      elif line.startswith("detail: "):
                          detail = line[len("detail: "):]
                      elif line.startswith("elapsed_ms: "):
                          elapsed = line[len("elapsed_ms: "):]
                  print(f"  {path.name}: elapsed={elapsed}ms phase={phase} detail={detail}")
          else:
              print("  none found")
          print()

          if not trace or not trace.exists():
              print("no chrome trace found")
              sys.exit(0)

          # the chrome trace is a json array with one object per line; the
          # last line may be truncated if mush was killed, so parse line by
          # line and tolerate a bad tail rather than failing the whole summary.
          draw_reasons = collections.Counter()
          stacks = collections.defaultdict(list)
          spans = collections.defaultdict(list)
          parsed = bad = 0
          for line in trace.open(errors="replace"):
              line = line.strip().rstrip(",")
              if not line or line[0] != "{":
                  continue
              try:
                  event = json.loads(line)
              except Exception:
                  bad += 1
                  continue
              parsed += 1
              name = event.get("name", "")
              cat = event.get("cat", "")
              args = event.get("args") or {}
              if name == "draw requested" or cat == "mush_tui::draw_reason":
                  draw_reasons[args.get("reason", "unknown")] += 1
              phase = event.get("ph")
              tid = event.get("tid", 0)
              if phase == "B":
                  stacks[tid].append((name, event.get("ts", 0.0)))
              elif phase == "E" and stacks[tid]:
                  start_name, start_ts = stacks[tid].pop()
                  spans[start_name].append(event.get("ts", 0.0) - start_ts)

          print(f"trace events: parsed={parsed} unparsable={bad}")
          print()

          print("draw reasons")
          if draw_reasons:
              for reason, count in draw_reasons.most_common():
                  print(f"  {count:8d} {reason}")
          else:
              print("  none found")
          print()

          print("trace span timings (wall clock)")
          for name in ["draw_panes", "llm_stream", "tool_exec", "agent_turn"]:
              values = sorted(spans.get(name, []))
              if not values:
                  continue
              count = len(values)
              total = sum(values) / 1000.0
              def q(frac):
                  return values[min(count - 1, int(frac * count))] / 1000.0
              print(
                  f"  {name:12} n={count:8d} total={total:10.1f}ms "
                  f"p50={q(0.50):7.3f}ms p90={q(0.90):7.3f}ms "
                  f"p99={q(0.99):7.3f}ms max={values[-1] / 1000.0:7.3f}ms"
              )
          PY

          echo "" >&2
          echo "perf bundle saved: $data_dir" >&2
          echo "samply profile: $profile" >&2
          echo "summary: $data_dir/perf.summary.txt" >&2
          echo "open the profile with: nix run .#perf-load" >&2
          echo "             or:       samply load $profile" >&2
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
