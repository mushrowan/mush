#       # global instructions
#       - british spelling
#     '';
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

  skillType = lib.types.either lib.types.str skillModule;

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

  promptModule = lib.types.submodule {
    options = {
      description = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          optional one-line description shown in the slash menu and
          `@`-picker. when null the picker shows just the name
        '';
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
    else if prompt.description == null
    then prompt.content
    else ''
      ---
      description: ${prompt.description}
      ---

      ${prompt.content}
    '';

  promptToFile = name: prompt: {
    "mush/prompts/${name}.md".text = promptToText name prompt;
  };

  schemaModule = nixcfg.mkModule {
    inherit schema;
    prefix = ["programs"];
    settingsAttr = "settings";
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
            summarise = builtins.readFile ./prompts/summarise.md;

            review = '''
              review code for issues
              review $1 for issues and suggest fixes.
            ''';
          }
        '';
        description = ''
          prompt templates to install in ~/.config/mush/prompts/. each
          key becomes `<name>.md` under that directory. invoked via
          `@name<tab>` or `/name args...`. the first non-empty line of
          the body doubles as the description shown in the slash menu
          and `@`-picker. supports `$1`, `$2`, `$@` / `$ARGUMENTS`
          placeholders for args
        '';
      };
    };
  };

  clean = attrs: let
    filtered = lib.filterAttrs (_: v: v != null) attrs;
    mapped = lib.mapAttrs (_: v:
      if lib.isAttrs v
      then clean v
      else v)
    filtered;
  in
    lib.filterAttrs (_: v: !(lib.isAttrs v && v == {})) mapped;

  parsedSchema = builtins.fromJSON (builtins.readFile schema);
  convertedSettings = nixcfg.toConfigAttrs {} parsedSchema cfg.settings;

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
