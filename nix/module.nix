# home-manager module for mush
#
# options under `programs.mush.settings.*` are generated from the rust
# `Config` struct via nixcfg + a JSON Schema checked in at
# `nix/config-schema.json`. a flake check re-runs the emitter and
# diffs to stop drift.
#
# anything that isn't part of the runtime config (package choice,
# AGENTS.md content, skill installer) stays hand-written at the
# top level.
#
# usage:
#   programs.mush = {
#     enable = true;
#     settings = {
#       model = "claude-sonnet-4-20250514";
#       thinking = "high";
#       theme.assistant = "#7aa2f7";
#       mcp.git = {
#         type = "local";
#         command = ["uvx" "mcp-server-git"];
#       };
#     };
#     agentsMd = ''
#       # global instructions
#       - british spelling
#     '';
#     skills.rust-idioms = {
#       description = "Rust idioms and best practices";
#       content = "prefer Result over unwrap";
#     };
#   };
self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.mush;
  schema = ./config-schema.json;
  nixcfg = self.inputs.nixcfg.lib;
  tomlFormat = pkgs.formats.toml {};

  # -- skills installer -------------------------------------------------

  # structured skill with separate description and content
  skillModule = lib.types.submodule {
    options = {
      description = lib.mkOption {
        type = lib.types.str;
        description = "short description shown in skill listings";
        example = "Rust idioms and best practices";
      };

      content = lib.mkOption {
        type = lib.types.str;
        description = "skill body (markdown below the frontmatter)";
        example = ''
          ## When to use me
          Use this skill when writing Rust code.

          ## Conventions
          - prefer Result over unwrap
        '';
      };

      files = lib.mkOption {
        type = lib.types.attrsOf (lib.types.either lib.types.path lib.types.str);
        default = {};
        description = "additional files in the skill directory. paths are linked, strings are written inline";
        example = {
          "references/api.md" = ./skills/my-skill/references/api.md;
          "references/inline.md" = "# inline content";
        };
      };
    };
  };

  # accept either a raw SKILL.md string (with frontmatter) or a
  # structured { description, content } attrset
  skillType = lib.types.either lib.types.str skillModule;

  # normalise either form into the final SKILL.md text
  skillToText = name: skill:
    if lib.isString skill
    then skill
    else ''
      ---
      name: ${name}
      description: ${skill.description}
      ---

      ${skill.content}
    '';

  # build xdg.configFile entries for a skill (SKILL.md + extra files)
  skillToFiles = name: skill:
    {
      "mush/skills/${name}/SKILL.md".text = skillToText name skill;
    }
    // (
      if lib.isString skill
      then {}
      else
        lib.mapAttrs' (
          path: value:
            lib.nameValuePair "mush/skills/${name}/${path}" (
              if builtins.isPath value
              then {source = value;}
              else {text = value;}
            )
        ) (skill.files or {})
    );

  # -- mcp submodule (manual override) ----------------------------------
  #
  # schemars emits McpServerConfig as an `oneOf` over Local/Remote tagged
  # variants. nixcfg's mapType picks the outer-struct submodule shape and
  # can't render the tagged union natively yet, so we hand-roll the nix
  # type here and slot it in via `overrides` below. keeps mcp usable as
  # `programs.mush.settings.mcp.<name> = { type = "local"; ...; }` the
  # same way it did pre-migration

  mcpServerModule = lib.types.submodule {
    options = {
      type = lib.mkOption {
        type = lib.types.enum ["local" "remote"];
        description = "transport type for the MCP server";
      };

      command = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        example = ["uvx" "mcp-server-git"];
        description = "command and arguments for local servers";
      };

      url = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "https://mcp.example.com/sse";
        description = "URL for remote servers";
      };

      headers = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = {};
        description = "HTTP headers for remote servers";
      };

      enabled = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "whether this server is enabled";
      };

      timeout = lib.mkOption {
        type = lib.types.ints.positive;
        default = 30;
        description = "request timeout in seconds";
      };

      environment = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = {};
        description = "environment variables for the server process";
      };
    };
  };

  # -- schema → nix module ----------------------------------------------

  schemaModule = nixcfg.mkModule {
    inherit schema;
    prefix = ["programs"];
    settingsAttr = "settings";
    overrides = {
      # mcp and keys are free-form maps whose values need custom nix types
      # that don't round-trip cleanly through schemars yet. the schema
      # still describes them for cli/env/config consumers
      mcp.type = lib.types.attrsOf mcpServerModule;
      keys.type = lib.types.attrsOf (lib.types.either lib.types.str (lib.types.listOf lib.types.str));
    };
  };

  # -- config.toml generation -------------------------------------------

  # remove nulls + empty attrsets so the emitted toml stays tidy
  clean = attrs: let
    filtered = lib.filterAttrs (_: v: v != null) attrs;
    mapped = lib.mapAttrs (_: v:
      if lib.isAttrs v
      then clean v
      else v)
    filtered;
  in
    lib.filterAttrs (_: v: !(lib.isAttrs v && v == {})) mapped;

  # user-visible settings attrset (matches the on-disk `config.toml`
  # layout one-to-one). keep api-key *paths* out of the toml because
  # mush reads keys from env vars; the paths on disk are purely nix-side
  # metadata for future secret-wiring (agenix / sops etc)
  tomlSettings = clean (
    (removeAttrs cfg.settings ["apiKeys"])
    // lib.optionalAttrs (cfg.settings.apiKeys or {} != {}) {
      api_keys = clean {
        # secret suffix stripped: on-disk toml uses the original field name
        anthropic = cfg.settings.apiKeys.anthropicPath or null;
        openrouter = cfg.settings.apiKeys.openrouterPath or null;
        openai = cfg.settings.apiKeys.openaiPath or null;
      };
    }
  );

  localModule = _: {
    options.programs.mush = {
      package = lib.mkOption {
        type = lib.types.package;
        inherit (self.packages.${pkgs.system}) default;
        defaultText = lib.literalExpression "pkgs.mush";
        description = "the mush package to install";
      };

      agentsMd = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = ''
          # global agent instructions
          - british spelling
          - no em dashes or semicolons
        '';
        description = ''
          content for ~/.config/mush/AGENTS.md (user-global agent instructions).
          mush loads this alongside any project-level AGENTS.md files
        '';
      };

      skills = lib.mkOption {
        type = lib.types.attrsOf skillType;
        default = {};
        example = lib.literalExpression ''
          {
            # raw SKILL.md text (e.g. from builtins.readFile)
            jj = builtins.readFile ./skills/jj.md;

            # structured form (frontmatter is generated)
            rust-idioms = {
              description = "Rust idioms and best practices";
              content = '''
                ## When to use me
                Use when writing or reviewing Rust code.
              ''';
            };
          }
        '';
        description = ''
          skills to install in ~/.config/mush/skills/. each key becomes a
          subdirectory containing a SKILL.md. accepts either a raw markdown
          string (with yaml frontmatter) or a { description, content } set
        '';
      };
    };

    config = lib.mkIf cfg.enable {
      home.packages = [cfg.package];

      xdg.configFile =
        {
          "mush/config.toml" = lib.mkIf (tomlSettings != {}) {
            source = tomlFormat.generate "mush-config" tomlSettings;
          };
        }
        // lib.optionalAttrs (cfg.agentsMd != null) {
          "mush/AGENTS.md".text = cfg.agentsMd;
        }
        // lib.concatMapAttrs skillToFiles cfg.skills;
    };
  };
in {
  imports = [(schemaModule {}) localModule];
}
