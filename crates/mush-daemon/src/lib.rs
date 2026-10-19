pub mod build;
pub mod client;
pub mod election;
pub mod protocol;
pub mod supervisor;
pub mod worker;

pub use build::build_id;
pub use client::{ConnectError, DaemonConnection};
pub use protocol::{Cursor, Frame, Reply};
pub use supervisor::{
    ChildDescriptor, OpenTarget, Relaunch, Supervisor, SupervisorReply, SupervisorRequest,
    WorkerDescriptor, daemon_socket_path, workers_dir,
};
pub use worker::{Descriptor, bind, serve};

#[cfg(test)]
mod test_support;
