use crate::fork::Env;
use alloy::primitives::Bytes;
use alloy::sol_types::SolCall;
use revm::interpreter::InstructionResult;
use revm_inspectors::tracing::CallTraceArena;

/// Raw result of a forked EVM call.
///
/// This is the crate's own type, replacing `foundry_evm::executors::RawCallResult`
/// with just the fields downstream consumers actually use: the EVM exit status,
/// the return bytes, gas consumed, the call-trace arena (when tracing captured
/// one), and the environment the call executed against.
#[derive(Debug, Clone)]
pub struct RawCallResult {
    /// The EVM exit status of the call.
    pub exit_reason: InstructionResult,
    /// Whether the call reverted.
    pub reverted: bool,
    /// The raw returned bytes of the call (empty on halt/create).
    pub result: Bytes,
    /// Gas consumed by the call.
    pub gas_used: u64,
    /// The call-trace arena collected by the tracing inspector, if any.
    pub traces: Option<CallTraceArena>,
    /// The environment (cfg/block/tx) the call executed against.
    pub env: Env,
}

/// Result of an alloy-typed call: the raw EVM result plus the ABI-decoded return.
pub struct ForkTypedReturn<C: SolCall> {
    /// The raw EVM call result, including traces and exit reason.
    pub raw: RawCallResult,
    /// The ABI-decoded return value.
    pub typed_return: C::Return,
}
