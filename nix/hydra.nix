{inputs, ...}: let
  withMaintainer = builtins.mapAttrs (_: job:
    job.overrideAttrs (old: {
      meta =
        (old.meta or {})
        // {
          maintainer = "roarch@proton.me";
        };
    }));
in {
  # keep hydra's public job surface explicit. val only has an
  # x86_64-linux builder, so do not expose every flake check system here.
  flake.hydraJobs = {
    x86_64-linux = withMaintainer inputs.self.checks.x86_64-linux;
  };
}
