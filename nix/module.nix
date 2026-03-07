# home-manager module for mush
#
# usage (import the module, then):
#   programs.mush = {
#     enable = true;
#     model = "claude-sonnet-4-20250514";
#     thinking = true;
#     theme.assistant = "#7aa2f7";
#     mcp.git = {
#       type = "local";
#       command = ["uvx" "mcp-server-git"];
#     };
#   };
self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.mush;
  tomlFormat = pkgs.formats.toml {};

  colourType = lib.types.nullOr lib.types.str;
  colourOption = description:
    lib.mkOption {
      type = colourType;
      default = null;
      inherit description;
      example = "#7aa2f7";
    };

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

  # remove null values and empty attrsets for clean toml
  clean = attrs: let
    filtered = lib.filterAttrs (_: v: v != null) attrs;
    mapped = lib.mapAttrs (_: v:
      if lib.isAttrs v
      then clean v
      else v)
    filtered;
  in
    lib.filterAttrs (_: v: !(lib.isAttrs v && v == {})) mapped;

  mcpServerToAttrs = _: srv: let
    base =
      {inherit (srv) type enabled timeout;}
      // lib.optionalAttrs (srv.environment != {}) {
        inherit (srv) environment;
      };
    transport =
      if srv.type == "local"
      then {inherit (srv) command;}
      else
        {inherit (srv) url;}
        // lib.optionalAttrs (srv.headers != {}) {
          inherit (srv) headers;
        };
  in
    base // transport;

  tomlSettings = clean (
    {
      inherit (cfg) model;
      inherit (cfg) thinking;
      max_tokens = cfg.maxTokens;
      max_turns = cfg.maxTurns;
      system_prompt = cfg.systemPrompt;
      log_filter = cfg.logFilter;
    }
    // lib.optionalAttrs (cfg.hintMode != "message") {
      hint_mode = cfg.hintMode;
    }
    // lib.optionalAttrs cfg.cacheTimer {
      cache_timer = true;
    }
    // lib.optionalAttrs (cfg.apiKeys.anthropic != null || cfg.apiKeys.openrouter != null) {
      api_keys = clean {
        inherit (cfg.apiKeys) anthropic openrouter;
      };
    }
    // (let
      t = clean cfg.theme;
    in
      lib.optionalAttrs (t != {}) {theme = t;})
    // lib.optionalAttrs (cfg.mcp != {}) {
      mcp = lib.mapAttrs mcpServerToAttrs cfg.mcp;
    }
    // cfg.settings
  );
in {
  options.programs.mush = {
    enable = lib.mkEnableOption "mush coding agent";

    package = lib.mkOption {
      type = lib.types.package;
      default =
        if cfg.embeddings
        then self.packages.${pkgs.system}.with-embeddings
        else self.packages.${pkgs.system}.default;
      defaultText = lib.literalExpression "pkgs.mush (with or without embeddings)";
      description = "the mush package to install";
    };

    embeddings = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        enable local embeddings for auto-context injection.
        uses EmbeddingGemma-300M (ONNX) to match relevant skills to queries.
        requires onnxruntime and downloads the model on first run
      '';
    };

    model = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "claude-sonnet-4-20250514";
      description = "default model to use";
    };

    thinking = lib.mkOption {
      type = lib.types.nullOr lib.types.bool;
      default = null;
      description = "enable extended thinking";
    };

    maxTokens = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      example = 8192;
      description = "maximum output tokens per response";
    };

    maxTurns = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      example = 20;
      description = "maximum agent turns per request";
    };

    systemPrompt = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "custom system prompt override";
    };

    hintMode = lib.mkOption {
      type = lib.types.enum ["message" "transform" "none"];
      default = "message";
      description = ''
        how to inject skill relevance hints.
        "message" prepends hints to user messages,
        "transform" re-evaluates before each LLM call,
        "none" disables hints
      '';
    };

    apiKeys = lib.mkOption {
      type = lib.types.submodule {
        options = {
          anthropic = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "anthropic API key (prefer ANTHROPIC_API_KEY env var)";
          };

          openrouter = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "openrouter API key (prefer OPENROUTER_API_KEY env var)";
          };
        };
      };
      default = {};
      description = ''
        API key overrides. prefer environment variables over config file
        for secrets (ANTHROPIC_API_KEY, OPENROUTER_API_KEY)
      '';
    };

    theme = lib.mkOption {
      type = lib.types.submodule {
        options = {
          user = colourOption "colour for user message labels";
          assistant = colourOption "colour for assistant message labels";
          system = colourOption "colour for system message labels";
          thinking = colourOption "colour for thinking/reasoning text";
          code = colourOption "colour for code blocks and inline code";
          heading = colourOption "colour for markdown headings";
          tool_running = colourOption "colour for running tool indicators";
          tool_done = colourOption "colour for completed tool indicators";
          tool_error = colourOption "colour for failed tool indicators";
          status = colourOption "colour for status bar model name";
          border = colourOption "colour for input box border";
        };
      };
      default = {};
      description = ''
        theme colour overrides. accepts colour names (red, blue, cyan),
        hex codes (#rrggbb), or 256-colour indices (196)
      '';
    };

    mcp = lib.mkOption {
      type = lib.types.attrsOf mcpServerModule;
      default = {};
      example = lib.literalExpression ''
        {
          git = {
            type = "local";
            command = ["uvx" "mcp-server-git"];
          };
          api = {
            type = "remote";
            url = "https://mcp.example.com/sse";
            timeout = 60;
          };
        }
      '';
      description = "MCP (Model Context Protocol) server configurations";
    };

    logFilter = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "mush=debug,warn";
      description = "tracing filter string (RUST_LOG env var takes priority over this)";
    };

    cacheTimer = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        show cache warmth countdown in the status bar and send desktop
        notifications when the prompt cache is about to expire
      '';
    };

    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = {};
      description = ''
        additional settings merged into config.toml. use this for options
        not yet exposed as typed module options
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [cfg.package];

    xdg.configFile."mush/config.toml" = lib.mkIf (tomlSettings != {}) {
      source = tomlFormat.generate "mush-config" tomlSettings;
    };
  };
}
