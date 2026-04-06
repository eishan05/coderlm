//! MCP (Model Context Protocol) transport layer for CodeRLM.
//!
//! Exposes the same `ops::*` service functions as the HTTP API, but through
//! the MCP tool-call protocol over stdio. This allows LLM agents to
//! communicate with the CodeRLM index via the standard MCP client/server
//! pattern instead of HTTP.

pub mod server;

#[cfg(test)]
mod tests;
