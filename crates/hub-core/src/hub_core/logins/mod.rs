//! The login model: whose subscription logins a call can run on, how they
//! are read from the vaults, cooled at their limits, and refused.

pub mod chatgpt_auth;
pub mod chatgpt_backend;
pub mod cooldown;
pub mod device_login;
pub mod errors;
pub mod limits;
pub mod resolve;
pub mod store;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
