//! Generic forked-EVM executor for the Rain ecosystem.
//!
//! A thin wrapper over [`revm`] + [`foundry_fork_db`] providing multi-fork RPC
//! forking, typed [`alloy`] calls, call tracing, and historical transaction
//! replay — the pieces previously supplied by `foundry-evm` (which is not on
//! crates.io). Extracted from `rainlang-eval` so that it, and its consumers,
//! can be published.

pub mod error;
pub mod fork;
pub mod result;

pub use error::{ForkCallError, ReplayTransactionError};
pub use fork::{Env, ForkId, Forker, NewForkedEvm};
pub use result::{ForkTypedReturn, RawCallResult};
