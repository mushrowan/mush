//! MCP (Model Context Protocol) client
//!
//! connects to MCP servers (local or remote) and exposes their
//! tools as `AgentTool` implementations for the agent loop.

mod config;
mod connection;
pub mod dynamic;
mod result;
mod tool;

pub use config::{McpServerConfig, McpServerType};
pub use connection::{McpConnection, McpManager};
pub use tool::McpTool;
