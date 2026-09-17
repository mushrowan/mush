pub mod build;
pub mod client;
pub mod protocol;
pub mod supervisor;
pub mod worker;

pub use build::build_id;
pub use client::{ConnectError, DaemonConnection};
pub use protocol::{Frame, Reply, Request, RequestId};
pub use supervisor::{
    OpenTarget, Supervisor, SupervisorReply, SupervisorRequest, WorkerDescriptor,
    daemon_socket_path, workers_dir,
};
pub use worker::{Descriptor, bind, serve};
