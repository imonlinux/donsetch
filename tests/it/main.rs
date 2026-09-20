//! The integration tests, as one crate.
//!
//! Every file directly under `tests/` is its own crate, so each one
//! linked the whole library and its dependency tree into a separate
//! binary, and re-generated the generic code it used from the library.
//! As modules of this one crate they share a single link. nextest still
//! runs every test in its own process, so the per-test isolation the
//! `DONSETCH_CACHE_DIR` tests rely on is unchanged.

mod auth_login;
mod bypass_live;
mod crawl_fresh_fetch;
mod daemon_boot_stays_alive;
mod egress_proxy;
mod mcp_tool_call_does_not_abort;
mod request_class;
mod revalidate_redirect;
mod secure_cookie_leak;
mod soak;
mod token_invariants;
