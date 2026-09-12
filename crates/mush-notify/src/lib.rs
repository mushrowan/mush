mod config;
mod dispatcher;
#[cfg(target_os = "linux")]
mod local;
mod policy;

pub use config::{Config, ConfigError, Mode};
pub use dispatcher::{ActivitySource, Clock, Dispatcher, Outcome, Transport};
pub use policy::{Activity, Completion, Decision, Policy, SessionActivity, Snapshot};

pub fn user_dispatcher() -> Option<Dispatcher> {
    #[cfg(target_os = "linux")]
    {
        local::user_dispatcher()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
