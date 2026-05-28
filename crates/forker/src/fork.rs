use crate::error::{ForkCallError, ReplayTransactionError};
use crate::result::{ForkTypedReturn, RawCallResult};
use alloy::consensus::Transaction as _;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::{AnyNetwork, TransactionResponse};
use alloy::primitives::{Address, BlockNumber, Bytes, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol_types::SolCall;
use foundry_fork_db::backend::SharedBackend;
use foundry_fork_db::cache::{BlockchainDb, BlockchainDbMeta};
use rain_error_decoding::AbiDecodedErrorType;
use revm::context::result::{ExecutionResult, Output, ResultAndState};
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::ContextTr;
use revm::database::CacheDB;
use revm::inspector::{InspectCommitEvm, InspectEvm};
use revm::interpreter::InstructionResult;
use revm::primitives::TxKind;
use revm::{Context, MainBuilder, MainContext};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::any::type_name;
use std::collections::HashMap;

/// Gas limit applied to every call. Kept deliberately huge (the cfg disables the
/// block gas limit / base-fee / EIP-3607 checks) so off-chain calls execute
/// regardless of network conditions, matching the old foundry-based executor.
const CALL_GAS_LIMIT: u64 = u64::MAX;

/// Identifies a fork by its RPC URL and the block it was pinned at.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ForkId {
    /// RPC URL of the forked network.
    pub url: String,
    /// Block the fork was created at, if pinned.
    pub block: Option<BlockNumber>,
}

impl ForkId {
    /// Creates a new fork identifier from a URL and optional block number.
    pub fn new(url: &str, block: Option<BlockNumber>) -> Self {
        Self {
            url: url.to_string(),
            block,
        }
    }
}

/// Configuration for creating a new forked EVM instance.
#[derive(Debug, Clone)]
pub struct NewForkedEvm {
    /// RPC URL of the network to fork.
    pub fork_url: String,
    /// Optional block number to fork from. Uses latest if `None`.
    pub fork_block_number: Option<BlockNumber>,
}

/// The EVM environment (cfg/block/tx) a call runs against.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// Chain/spec configuration.
    pub cfg: CfgEnv,
    /// Block environment.
    pub block: BlockEnv,
    /// Transaction environment.
    pub tx: TxEnv,
}

/// A single forked network: its lazily-cached database, the shared backend used
/// for replay block/tx lookups, and the cfg/block environment to execute against.
struct Fork {
    /// revm database layered over the RPC-backed shared backend; accumulates
    /// state committed by `*_committing` calls.
    db: CacheDB<SharedBackend>,
    /// Shared backend, retained for `get_full_block` / `get_transaction` lookups.
    backend: SharedBackend,
    /// cfg + block environment for this fork (tx is filled in per call).
    env: Env,
    /// RPC URL of the fork.
    url: String,
}

/// Thin multi-fork EVM wrapper providing read/write calls, tracing, and
/// historical transaction replay over [`revm`] + [`foundry_fork_db`].
pub struct Forker {
    forks: HashMap<ForkId, Fork>,
    active: Option<ForkId>,
}

impl Default for Forker {
    fn default() -> Self {
        Self::new_empty()
    }
}

/// Tracing config that records call frames (and their input calldata) without
/// step-level overhead — enough for consumers that read calldata sent to a
/// no-op "tracer" address out of the call-trace arena.
fn tracing_config() -> TracingInspectorConfig {
    TracingInspectorConfig::default_parity()
}

impl Forker {
    /// Creates a new empty `Forker` with no forks.
    pub fn new() -> eyre::Result<Forker> {
        Ok(Self::new_empty())
    }

    fn new_empty() -> Forker {
        Forker {
            forks: HashMap::new(),
            active: None,
        }
    }

    /// Builds the alloy provider, fetches the fork block + chain id, spawns the
    /// shared backend, and returns the assembled `Fork`.
    async fn create_fork(args: &NewForkedEvm, env: Option<Env>) -> Result<Fork, ForkCallError> {
        let NewForkedEvm {
            fork_url,
            fork_block_number,
        } = args;

        let url = fork_url
            .parse()
            .map_err(|e| ForkCallError::ExecutorError(format!("invalid fork url: {e}")))?;
        let provider = ProviderBuilder::new()
            .network::<AnyNetwork>()
            .connect_http(url);

        let chain_id = provider
            .get_chain_id()
            .await
            .map_err(|e| ForkCallError::ExecutorError(e.to_string()))?;

        let block_tag = match fork_block_number {
            Some(n) => BlockNumberOrTag::Number(*n),
            None => BlockNumberOrTag::Latest,
        };
        let rpc_block = provider
            .get_block_by_number(block_tag)
            .await
            .map_err(|e| ForkCallError::ExecutorError(e.to_string()))?
            .ok_or_else(|| ForkCallError::ExecutorError("fork block not found".to_string()))?;
        let header = &rpc_block.header;
        let block_number = header.number;

        let mut block_env = BlockEnv {
            number: U256::from(block_number),
            beneficiary: header.beneficiary,
            timestamp: U256::from(header.timestamp),
            gas_limit: header.gas_limit,
            basefee: header.base_fee_per_gas.unwrap_or_default(),
            difficulty: header.difficulty,
            ..Default::default()
        };
        block_env.prevrandao = header.mix_hash;

        // Configure for off-chain execution: lift the EIP-7825 per-tx gas cap and
        // disable the checks that would otherwise reject calls made with gas price
        // 0 from arbitrary senders, mirroring the old foundry-based executor.
        let mut cfg = CfgEnv::default();
        cfg.chain_id = chain_id;
        cfg.tx_gas_limit_cap = Some(u64::MAX);
        cfg.disable_base_fee = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_eip3607 = true;
        cfg.disable_nonce_check = true;

        let meta = BlockchainDbMeta::new(block_env.clone(), fork_url.clone());
        let db = BlockchainDb::new(meta, None);
        let backend =
            SharedBackend::spawn_backend_thread(provider, db, Some(BlockId::from(block_number)));

        let env = env.unwrap_or(Env {
            cfg,
            block: block_env,
            tx: TxEnv::default(),
        });

        Ok(Fork {
            db: CacheDB::new(backend.clone()),
            backend,
            env,
            url: fork_url.clone(),
        })
    }

    /// Creates a new `Forker` with a single active fork.
    pub async fn new_with_fork(
        args: NewForkedEvm,
        env: Option<Env>,
        _gas_limit: Option<u64>,
    ) -> Result<Forker, ForkCallError> {
        let fork = Self::create_fork(&args, env).await?;
        let id = ForkId::new(&args.fork_url, args.fork_block_number);
        let mut forks = HashMap::new();
        forks.insert(id.clone(), fork);
        Ok(Forker {
            forks,
            active: Some(id),
        })
    }

    /// Adds a new fork and selects it, or selects it if it already exists.
    pub async fn add_or_select(
        &mut self,
        args: NewForkedEvm,
        env: Option<Env>,
    ) -> Result<(), ForkCallError> {
        let id = ForkId::new(&args.fork_url, args.fork_block_number);
        if !self.forks.contains_key(&id) {
            let fork = Self::create_fork(&args, env).await?;
            self.forks.insert(id.clone(), fork);
        }
        self.active = Some(id);
        Ok(())
    }

    fn active_fork(&self) -> Result<&Fork, ForkCallError> {
        let id = self
            .active
            .as_ref()
            .ok_or_else(|| ForkCallError::ExecutorError("no active fork!".to_owned()))?;
        self.forks
            .get(id)
            .ok_or_else(|| ForkCallError::ExecutorError("no active fork!".to_owned()))
    }

    fn active_fork_mut(&mut self) -> Result<&mut Fork, ForkCallError> {
        let id = self
            .active
            .as_ref()
            .ok_or_else(|| ForkCallError::ExecutorError("no active fork!".to_owned()))?
            .clone();
        self.forks
            .get_mut(&id)
            .ok_or_else(|| ForkCallError::ExecutorError("no active fork!".to_owned()))
    }

    /// Performs a read-only call against the current fork state (not committed).
    pub fn call(
        &self,
        from_address: &[u8],
        to_address: &[u8],
        calldata: &[u8],
    ) -> Result<RawCallResult, ForkCallError> {
        let (from, to) = validate_addresses(from_address, to_address)?;
        let fork = self.active_fork()?;
        let tx = build_tx(
            &fork.env,
            from,
            TxKind::Call(to),
            Bytes::copy_from_slice(calldata),
            U256::ZERO,
        );
        let mut db = fork.db.clone();
        exec(&mut db, &fork.env, tx, false)
    }

    /// Performs a state-committing call against the current fork.
    pub fn call_committing(
        &mut self,
        from_address: &[u8],
        to_address: &[u8],
        calldata: &[u8],
        value: U256,
    ) -> Result<RawCallResult, ForkCallError> {
        let (from, to) = validate_addresses(from_address, to_address)?;
        let fork = self.active_fork_mut()?;
        let tx = build_tx(
            &fork.env,
            from,
            TxKind::Call(to),
            Bytes::copy_from_slice(calldata),
            value,
        );
        let placeholder = CacheDB::new(fork.backend.clone());
        let mut db = std::mem::replace(&mut fork.db, placeholder);
        let result = exec(&mut db, &fork.env, tx, true);
        fork.db = db;
        result
    }

    /// Calls the forked EVM without committing, using alloy typed arguments.
    pub async fn alloy_call<T: SolCall>(
        &self,
        from_address: Address,
        to_address: Address,
        call: T,
        decode_error: bool,
    ) -> Result<ForkTypedReturn<T>, ForkCallError> {
        let raw = self.call(
            from_address.as_slice(),
            to_address.as_slice(),
            &call.abi_encode(),
        )?;
        decode_typed(raw, decode_error).await
    }

    /// Writes to the forked EVM using alloy typed arguments.
    pub async fn alloy_call_committing<T: SolCall>(
        &mut self,
        from_address: Address,
        to_address: Address,
        call: T,
        value: U256,
        decode_error: bool,
    ) -> Result<ForkTypedReturn<T>, ForkCallError> {
        let raw = self.call_committing(
            from_address.as_slice(),
            to_address.as_slice(),
            &call.abi_encode(),
            value,
        )?;
        decode_typed(raw, decode_error).await
    }

    /// Rolls the active fork to a given block number (or leaves it unchanged if
    /// not provided), updating the block environment.
    pub fn roll_fork(
        &mut self,
        block_number: Option<BlockNumber>,
        _env: Option<Env>,
    ) -> Result<(), ForkCallError> {
        let fork = self.active_fork_mut()?;
        if let Some(block_number) = block_number {
            fork.env.block.number = U256::from(block_number);
        }
        Ok(())
    }

    /// Replays a historical transaction: forks at the block before the tx, replays
    /// every earlier transaction in that block, then executes the target tx.
    pub async fn replay_transaction(
        &mut self,
        tx_hash: B256,
    ) -> Result<RawCallResult, ForkCallError> {
        let (fork_url, backend) = {
            let fork = self.active_fork()?;
            (fork.url.clone(), fork.backend.clone())
        };

        let full_tx = backend.get_transaction(tx_hash).map_err(|e| {
            ReplayTransactionError::DatabaseError(tx_hash.to_string(), fork_url.clone(), e)
        })?;

        let block_number = full_tx.block_number.ok_or_else(|| {
            ReplayTransactionError::NoBlockNumberFound(tx_hash.to_string(), fork_url.clone())
        })?;

        let prev_block = block_number.checked_sub(1).ok_or_else(|| {
            ReplayTransactionError::GenesisBlockReplay(tx_hash.to_string(), fork_url.clone())
        })?;

        let block = backend.get_full_block(block_number).map_err(|e| {
            ReplayTransactionError::DatabaseError(block_number.to_string(), fork_url.clone(), e)
        })?;

        self.add_or_select(
            NewForkedEvm {
                fork_url: fork_url.clone(),
                fork_block_number: Some(prev_block),
            },
            None,
        )
        .await?;

        // Match the block environment to the block the transaction is in.
        {
            let fork = self.active_fork_mut()?;
            fork.env.block.number = U256::from(block_number);
            fork.env.block.timestamp = U256::from(block.header.timestamp);
            fork.env.block.beneficiary = block.header.beneficiary;
            fork.env.block.difficulty = block.header.difficulty;
            fork.env.block.prevrandao = block.header.mix_hash;
            fork.env.block.basefee = block.header.base_fee_per_gas.unwrap_or_default();
            fork.env.block.gas_limit = block.header.gas_limit;
        }

        // Replay every transaction mined before the target, committing state.
        for tx in block.transactions.txns() {
            let from = tx.from();
            if tx.tx_hash() == tx_hash {
                let res = match tx.kind() {
                    TxKind::Call(to) => self.call(from.as_slice(), to.as_slice(), tx.input())?,
                    TxKind::Create => self.call(from.as_slice(), &[0u8; 20], tx.input())?,
                };
                return Ok(res);
            }

            if let TxKind::Call(to) = tx.kind() {
                let _ =
                    self.call_committing(from.as_slice(), to.as_slice(), tx.input(), tx.value());
            }
        }

        Err(ForkCallError::ReplayTransactionError(
            ReplayTransactionError::TransactionNotFound(tx_hash.to_string(), fork_url),
        ))
    }
}

/// Decodes an alloy-typed return from a raw result, surfacing reverts as decoded
/// errors when requested.
async fn decode_typed<T: SolCall>(
    raw: RawCallResult,
    decode_error: bool,
) -> Result<ForkTypedReturn<T>, ForkCallError> {
    if decode_error && raw.exit_reason == InstructionResult::Revert {
        return Err(ForkCallError::AbiDecodedError(
            AbiDecodedErrorType::selector_registry_abi_decode(&raw.result, None).await?,
        ));
    }

    if !raw.exit_reason.is_ok() {
        return Err(raw.into());
    }

    let typed_return = T::abi_decode_returns(&raw.result).map_err(|e| {
        ForkCallError::TypedError(format!(
            "Call:{:?} Error:{:?} Raw:{:?}",
            type_name::<T>(),
            e,
            raw
        ))
    })?;
    Ok(ForkTypedReturn { raw, typed_return })
}

/// Builds a tx env from the fork env plus the call specifics, with gas price 0.
fn build_tx(env: &Env, caller: Address, kind: TxKind, data: Bytes, value: U256) -> TxEnv {
    TxEnv {
        caller,
        kind,
        data,
        value,
        gas_price: 0,
        gas_limit: CALL_GAS_LIMIT,
        chain_id: Some(env.cfg.chain_id),
        ..Default::default()
    }
}

/// Runs a transaction against `db` with a tracing inspector, optionally
/// committing the resulting state, and converts the outcome into a `RawCallResult`.
fn exec(
    db: &mut CacheDB<SharedBackend>,
    env: &Env,
    tx: TxEnv,
    commit: bool,
) -> Result<RawCallResult, ForkCallError> {
    let inspector = TracingInspector::new(tracing_config());
    // Move the db into the context by swapping with a cheap empty placeholder
    // that shares the same backend; restore it afterwards.
    let backend = db.db.clone();
    let owned_db = std::mem::replace(db, CacheDB::new(backend));

    let mut evm = Context::mainnet()
        .with_db(owned_db)
        .with_block(env.block.clone())
        .with_cfg(env.cfg.clone())
        .build_mainnet_with_inspector(inspector);

    let exec_result: ExecutionResult = if commit {
        evm.inspect_tx_commit(tx.clone())
            .map_err(|e| ForkCallError::ExecutorError(e.to_string()))?
    } else {
        let ResultAndState { result, .. } = evm
            .inspect_tx(tx.clone())
            .map_err(|e| ForkCallError::ExecutorError(e.to_string()))?;
        result
    };

    // Restore the (possibly mutated) db back into the caller's slot, then take
    // the inspector back out to read its traces.
    let placeholder = CacheDB::new(db.db.clone());
    *db = std::mem::replace(evm.ctx.db_mut(), placeholder);
    let traces = evm.into_inspector().into_traces();

    let gas_used = exec_result.gas_used();
    let (exit_reason, result) = match exec_result {
        ExecutionResult::Success { reason, output, .. } => {
            let data = match output {
                Output::Call(data) => data,
                Output::Create(data, _) => data,
            };
            (InstructionResult::from(reason), data)
        }
        ExecutionResult::Revert { output, .. } => (InstructionResult::Revert, output),
        ExecutionResult::Halt { reason, .. } => (InstructionResult::from(reason), Bytes::new()),
    };

    Ok(RawCallResult {
        reverted: !exit_reason.is_ok(),
        exit_reason,
        result,
        gas_used,
        traces: Some(traces),
        env: Env {
            cfg: env.cfg.clone(),
            block: env.block.clone(),
            tx,
        },
    })
}

/// Validates that both addresses are exactly 20 bytes and converts them.
fn validate_addresses(from: &[u8], to: &[u8]) -> Result<(Address, Address), ForkCallError> {
    if from.len() != 20 || to.len() != 20 {
        return Err(ForkCallError::ExecutorError("invalid address!".to_owned()));
    }
    Ok((Address::from_slice(from), Address::from_slice(to)))
}
