//! Outbound connectors for zirv-flow.
//!
//! Every outbound HTTP call goes through [`http::GuardedClient`], which enforces the egress
//! policy of `zirv_flow_core::egress` on every request and redirect. This crate exposes no plain
//! `reqwest::Client`, so a call site cannot skip the guard by accident.
//!
//! A source reaches the network only through its [`sources::SourceContext`], and [`runtime`] is
//! the machinery behind it: auth, retries, a rate limit and pagination.

pub mod http;
pub mod manifest;
pub mod runtime;
pub mod sources;
