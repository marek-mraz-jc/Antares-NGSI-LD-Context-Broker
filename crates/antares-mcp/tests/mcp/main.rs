// SPDX-License-Identifier: EUPL-1.2
//! The MCP surface's integration tests, one binary, over the fixture in
//! `common`: discovery and the tool list (`mcp_discovery`), the gate
//! (`mcp_gate`), errors (`mcp_errors`), result size and paging
//! (`mcp_output`), resources (`mcp_resources`), caching (`mcp_cache`) and
//! the transport (`mcp_transport`).

mod common;
mod engines;
mod mcp_cache;
mod mcp_discovery;
mod mcp_errors;
mod mcp_every;
mod mcp_gate;
mod mcp_output;
mod mcp_resources;
mod mcp_transport;
