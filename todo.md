# refactor todo

## enums over booleans
- [x] 1. `ToolResult.is_error` / `ToolResultMessage.is_error` → `ToolOutcome::Success | Error`
- [x] 2. `ThinkingContent.redacted: bool` → split into `Thinking` / `RedactedThinking` variants
- [x] 3. `Config.thinking: Option<bool>` → `Option<ThinkingLevel>` (enum already exists)
- [x] 4. `Option<bool>` config fields (`debug_cache`, `confirm_tools`) → plain `bool` with `#[serde(default)]`

## deduplication
- [x] 5. `print_mode` / `tui_mode` shared setup → `AppSetup` struct or builder
- [x] 6. `HintMode` defined in both `config` and `runner` → single enum in shared location

## blocking I/O in async
- [x] 7. `std::fs` in tool `execute()` fns (read, write, edit, ls) → `tokio::fs` or `spawn_blocking`

## stringly-typed values
- [x] 8. `ImageContent.mime_type: String` → `ImageMimeType` enum (jpeg, png, gif, webp)

## api safety
- [x] 9. `ApiKey` expose pattern → add `fn expose(&self) -> &str` method, remove `Deref` for secret access

## minor cleanup
- [x] 10. `provider_api_keys` manual HashMap build → `ApiKeys::to_map()` (done in #5)

---

## round 2

### lint hygiene
- [x] 11. `#[allow]` → `#[expect]` — 2 instances in anthropic.rs
- [x] 12. `#[must_use]` on key types/fns — `ToolResult`, `ToolOutcome`, `ApiKey`, `Temperature`, `BaseUrl`, `ImageMimeType`, `ApiKeys::to_map()`
- [x] 13. explicit `let _ =` for discarded results — `stdout().flush()`, `store.save()`, `fs::create_dir_all()`, `fs::write()`

### stringly-typed errors
- [x] 14. `ProviderError::MissingApiKey(String)` → `MissingApiKey(Provider)`
- [x] 15. `ProviderError::Other(String)` → `InvalidHeader(#[from])` + `ApiError { api, status, body }`

### let chains
- [x] 16. audited nested `if let` patterns — few real opportunities, codebase already uses let chains where appropriate

### round 3

#### flexible APIs
- [x] 17. `push_system_message` / `push_user_message` take `impl Into<String>` — removes `.into()` at call sites
- [x] 18. `branch_with_summary` takes `impl Into<String>` for the summary param

#### derive & Default
- [x] 19. `ApiRegistry` `#[derive(Default)]`, `new()` delegates to default
- [x] 20. `HookRunner` `#[derive(Default)]`, `new()` delegates to default

#### Option combinators
- [x] 21. `resolve_thinking`: nested if-let-else → `.copied().or()` chain

#### clone audit
- [x] 22. audited runner.rs clones — all necessary (ownership boundaries, Arc, multi-call closures)

### round 4

#### non_exhaustive
- [ ] 23. `#[non_exhaustive]` on cross-crate enums likely to grow: `ProviderError`, `AgentEvent`, `StreamEvent`, `StopReason`

#### strum for enum Display
- [x] 24. audited manual Display impls — all have custom logic (ApiKey redacted, Provider::Custom, ImageMimeType mime strings), strum wouldn't help

#### LazyLock for statics
- [x] 25. audited — no regex, model catalogue reads user file each call so can't be static. no lazy_static/once_cell deps to replace

#### ecosystem crates
- [x] 26. `cargo-deny` — added `deny.toml` with license/advisory/ban/source checks, integrated via `craneLib.cargoDeny` in nix checks
- [x] 27. audited others: `parking_lot` (only 3 brief-lock mutexes, marginal), `dashmap` (no concurrent hashmaps), `const_format` (no const string building), `bon` (no complex builders), `strum` (all Display impls have custom logic)
