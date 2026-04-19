//! emit the nix-facing JSON Schema for mush's `Config` type
//!
//! run via `cargo run -p mush-cli --bin mush-config-schema`. the output
//! is checked in at `nix/config-schema.json` and consumed by
//! `nix/module.nix` through `nixcfg.lib.mkModule`. a flake check
//! re-runs this binary and diffs against the committed file to catch
//! drift when `Config` or any nested type changes.
//!
//! the schema follows the nixcfg extension spec (v1):
//! - `x-nixcfg-name` = "mush" and `x-nixcfg-config-format` = "toml" set
//!   via `#[schemars(extend(...))]` on the `Config` struct
//! - `x-nixcfg-secret` on the `ApiKey` newtype propagates to fields so
//!   they turn into `nullOr path` in nix with a `_path` suffix
//!
//! defaults are merged in from `Config::default()` so nix options
//! carry the same defaults the rust side already uses

use mush_cli::config::Config;

fn main() {
    println!("{}", nixcfg::emit::<Config>("mush"));
}
