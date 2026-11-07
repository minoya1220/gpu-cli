//! Ported Python tests, one module per Python test file (`tests/test_X.py`
//! maps to `X.rs`, keeping each test's name).

mod automation;
mod cli;
mod editors;
mod expiry;
mod expiry_launch;
mod gcp;
mod gcp_prices;
mod instances;
mod lifecycle;
mod listing;
mod picker;
mod repo_cache;
mod repos;
mod setup;
mod uptime;
mod wizard;

pub(crate) mod gcp_transport;
