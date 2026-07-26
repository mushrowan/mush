//! MCP (Model Context Protocol) client
//!
//! connects to MCP servers (local or remote) and exposes their
//! tools as `AgentTool` implementations for the agent loop.

pub mod catalogue;
mod config;
mod connection;
pub mod dynamic;
mod manager;
pub mod report;
mod result;
pub mod server;
mod tool;

pub use catalogue::{McpCatalogue, McpToolInfo};
pub use config::{McpServerConfig, McpServerType};
pub use connection::McpConnection;
pub use manager::McpManager;
pub use server::{McpServer, McpServerStatus, ServerState};
pub use tool::McpTool;
