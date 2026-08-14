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
  flake.hydraJobs = {
    x86_64-linux = withMaintainer inputs.self.checks.x86_64-linux;
  };
}
