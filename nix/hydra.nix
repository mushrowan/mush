{inputs, ...}: {
  # keep hydra's public job surface explicit. val only has an
  # x86_64-linux builder, so do not expose every flake check system here.
  flake.hydraJobs = {
    x86_64-linux = inputs.self.checks.x86_64-linux;
  };
}
