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
#     prompts.summarise = {
#       description = "summarise the given jj change";
#       content = "summarise change $1 in one paragraph.";
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

  # -- prompt template installer ----------------------------------------

  # a prompt template is either a raw markdown string (possibly with
  # yaml frontmatter containing `description: ...`) or a structured
  # { description, content } attrset from which we synthesise the
  # frontmatter. unlike skills, prompts are single flat files under
  # `~/.config/mush/prompts/<name>.md` so no `files` attribute
  promptModule = lib.types.submodule {
    options = {
      description = lib.mkOption {
        type = lib.types.str;
        description = "one-line description shown in the /command picker";
        example = "summarise the given jj change";
      };

      content = lib.mkOption {
        type = lib.types.str;
        description = ''
          prompt body. supports positional placeholders `$1`, `$2`, ...,
          and `$@` / `$ARGUMENTS` for all args joined. invoked via
          `@name<tab>` or `/name args...`
        '';
        example = ''
          summarise change $1 in one paragraph. no markdown, prose only.
        '';
      };
    };
  };

  promptType = lib.types.either lib.types.str promptModule;

  promptToText = _name: prompt:
    if lib.isString prompt
    then prompt
    else ''
      ---
      description: ${prompt.description}
      ---

      ${prompt.content}
    '';

  promptToFile = name: prompt: {
    "mush/prompts/${name}.md".text = promptToText name prompt;
  };

  # -- schema → nix module ----------------------------------------------

  schemaModule = nixcfg.mkModule {
    inherit schema;
    prefix = ["programs"];
    settingsAttr = "settings";
    overrides = {
      # `keys` is mush's keybindings map (action → key string or list of
      # key strings). schemars can't infer the value shape from the
      # untyped HashMap so we pin it here
      keys.type = lib.types.attrsOf (lib.types.either lib.types.str (lib.types.listOf lib.types.str));
    };

    # nix-only options that sit alongside `enable` (not part of the
    # rust-side Config). package selection, AGENTS.md content, and the
    # skills installer all belong here rather than in `settings`
    topLevelExtraOverrides = {
      package = {
        type = lib.types.package;
        inherit (self.packages.${pkgs.system}) default;
        defaultText = lib.literalExpression "pkgs.mush";
        description = "the mush package to install";
      };

      agentsMd = {
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

      skills = {
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

      prompts = {
        type = lib.types.attrsOf promptType;
        default = {};
        example = lib.literalExpression ''
          {
            # raw markdown (possibly with yaml frontmatter)
            summarise = builtins.readFile ./prompts/summarise.md;

            # structured form: frontmatter is generated
            review = {
              description = "review code for issues";
              content = "review $1 for issues and suggest fixes.";
            };
          }
        '';
        description = ''
          prompt templates to install in ~/.config/mush/prompts/. each
          key becomes `<name>.md` under that directory. invoked via
          `@name<tab>` or `/name args...`. accepts either a raw markdown
          string (with yaml frontmatter) or a { description, content } set.
          supports `$1`, `$2`, `$@` / `$ARGUMENTS` placeholders for args
        '';
      };
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

  # nixcfg renders schema properties as camelCase nix options (its default
  # naming convention), so cfg.settings carries `cacheTimer`, `statusBar`,
  # `postCompaction`, etc. the on-disk config.toml that mush's rust loader
  # deserialises expects snake_case. `toConfigAttrs` walks the schema,
  # renames recursively (including nested objects like status_bar and
  # hooks), and applies the secret `_path` suffix convention. without
  # this, every nix-managed setting was silently dropped on load
  parsedSchema = builtins.fromJSON (builtins.readFile schema);
  convertedSettings = nixcfg.toConfigAttrs {} parsedSchema cfg.settings;

  # user-visible settings attrset (matches the on-disk `config.toml`
  # layout one-to-one). override `api_keys` with mush's bespoke layout:
  # nixcfg would write `api_keys.anthropic_path`, but the rust side reads
  # `api_keys.anthropic` and lets the value stand in for a key/path
  tomlSettings = clean (
    (removeAttrs convertedSettings ["api_keys"])
    // lib.optionalAttrs (cfg.settings.apiKeys or {} != {}) {
      api_keys = clean {
        anthropic = cfg.settings.apiKeys.anthropicPath or null;
        openrouter = cfg.settings.apiKeys.openrouterPath or null;
        openai = cfg.settings.apiKeys.openaiPath or null;
      };
    }
  );

  # the schemaModule provides options + the `enable` toggle; this
  # second module wires up the actual file outputs (config.toml,
  # AGENTS.md, skills directory)
  configModule = _: {
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
        // lib.concatMapAttrs skillToFiles cfg.skills
        // lib.concatMapAttrs promptToFile cfg.prompts;
    };
  };
in {
  imports = [(schemaModule {}) configModule];
}
