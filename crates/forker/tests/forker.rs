//! Integration tests for the `Forker`.
//!
//! The no-network tests exercise the error paths directly. The remaining tests
//! spin up a local `anvil` node (via alloy's node bindings) and fork it, so they
//! require `anvil` on PATH (provided by the rainix dev shell).

use alloy::node_bindings::Anvil;
use alloy::primitives::{address, bytes, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use rain_forker::{ForkCallError, Forker, NewForkedEvm};

const ZERO: [u8; 20] = [0u8; 20];

sol! {
    interface Demo {
        function get() external returns (uint256);
    }
}

/// Runtime that returns the constant 42 for any call.
fn const42_runtime() -> alloy::primitives::Bytes {
    bytes!("602a60005260206000f3")
}

/// Runtime that always reverts with empty data.
fn revert_runtime() -> alloy::primitives::Bytes {
    bytes!("60006000fd")
}

const CONST_ADDR: Address = address!("00000000000000000000000000000000000000bb");
const REVERT_ADDR: Address = address!("00000000000000000000000000000000000000cc");

/// Spawns anvil, injects `code` at `addr`, and returns the endpoint URL.
async fn anvil_with_code(
    addr: Address,
    code: alloy::primitives::Bytes,
) -> (alloy::node_bindings::AnvilInstance, String) {
    let anvil = Anvil::new().spawn();
    let provider = ProviderBuilder::new().connect_http(anvil.endpoint().parse().unwrap());
    provider
        .raw_request::<_, ()>("anvil_setCode".into(), (addr, code))
        .await
        .unwrap();
    let url = anvil.endpoint();
    (anvil, url)
}

/// Address of the EVM identity precompile (0x..04), which returns its input.
fn identity_precompile() -> [u8; 20] {
    let mut addr = [0u8; 20];
    addr[19] = 0x04;
    addr
}

/// Minimal counter contract runtime: loads slot 0, adds 1, stores it back, and
/// returns the new value. `call_committing` therefore increments persistently;
/// `call` computes the next value but discards the write.
fn counter_runtime() -> alloy::primitives::Bytes {
    bytes!("6000546001018060005560005260206000f3")
}

const COUNTER_ADDR: Address = address!("00000000000000000000000000000000000000aa");

#[test]
fn test_call_invalid_from_address() {
    let forker = Forker::new().unwrap();
    let result = forker.call(&[0u8; 19], &ZERO, &[]);
    assert!(
        matches!(result, Err(ForkCallError::ExecutorError(ref msg)) if msg == "invalid address!")
    );
}

#[test]
fn test_call_invalid_to_address() {
    let forker = Forker::new().unwrap();
    let result = forker.call(&ZERO, &[0u8; 21], &[]);
    assert!(
        matches!(result, Err(ForkCallError::ExecutorError(ref msg)) if msg == "invalid address!")
    );
}

#[test]
fn test_call_empty_addresses() {
    let forker = Forker::new().unwrap();
    let result = forker.call(&[], &[], &[]);
    assert!(
        matches!(result, Err(ForkCallError::ExecutorError(ref msg)) if msg == "invalid address!")
    );
}

#[test]
fn test_call_no_active_fork() {
    let forker = Forker::new().unwrap();
    let result = forker.call(&ZERO, &identity_precompile(), &[]);
    assert!(
        matches!(result, Err(ForkCallError::ExecutorError(ref msg)) if msg == "no active fork!")
    );
}

#[test]
fn test_call_committing_invalid_address() {
    let mut forker = Forker::new().unwrap();
    let result = forker.call_committing(&[0u8; 19], &ZERO, &[], U256::ZERO);
    assert!(
        matches!(result, Err(ForkCallError::ExecutorError(ref msg)) if msg == "invalid address!")
    );
}

#[test]
fn test_roll_fork_no_active_fork() {
    let mut forker = Forker::new().unwrap();
    let result = forker.roll_fork(Some(100), None);
    assert!(
        matches!(result, Err(ForkCallError::ExecutorError(ref msg)) if msg == "no active fork!")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_fork_identity_precompile() {
    let anvil = Anvil::new().spawn();
    let forker = Forker::new_with_fork(
        NewForkedEvm {
            fork_url: anvil.endpoint(),
            fork_block_number: None,
        },
        None,
        None,
    )
    .await
    .unwrap();

    // The identity precompile echoes its input back as the return data.
    let input = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let res = forker.call(&ZERO, &identity_precompile(), &input).unwrap();

    assert!(res.exit_reason.is_ok());
    assert!(!res.reverted);
    assert_eq!(res.result.as_ref(), &input);
    // Tracing captured the top-level call frame.
    let traces = res.traces.expect("traces present");
    assert!(!traces.nodes().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_commit_persists_and_reads_are_isolated() {
    let anvil = Anvil::new().spawn();
    let provider = ProviderBuilder::new().connect_http(anvil.endpoint().parse().unwrap());

    // Inject the counter contract's runtime code before forking.
    provider
        .raw_request::<_, ()>("anvil_setCode".into(), (COUNTER_ADDR, counter_runtime()))
        .await
        .unwrap();

    let mut forker = Forker::new_with_fork(
        NewForkedEvm {
            fork_url: anvil.endpoint(),
            fork_block_number: None,
        },
        None,
        None,
    )
    .await
    .unwrap();

    let read_u256 = |bytes: &[u8]| U256::from_be_slice(bytes);

    // Two committing calls increment persistent storage: 0 -> 1 -> 2.
    let r1 = forker
        .call_committing(&ZERO, COUNTER_ADDR.as_slice(), &[], U256::ZERO)
        .unwrap();
    assert_eq!(read_u256(&r1.result), U256::from(1));
    let r2 = forker
        .call_committing(&ZERO, COUNTER_ADDR.as_slice(), &[], U256::ZERO)
        .unwrap();
    assert_eq!(read_u256(&r2.result), U256::from(2));

    // Read-only calls compute 2 + 1 = 3 but do NOT persist, so repeating gives 3.
    let read_a = forker.call(&ZERO, COUNTER_ADDR.as_slice(), &[]).unwrap();
    assert_eq!(read_u256(&read_a.result), U256::from(3));
    let read_b = forker.call(&ZERO, COUNTER_ADDR.as_slice(), &[]).unwrap();
    assert_eq!(read_u256(&read_b.result), U256::from(3));

    // The committed state is still 2, so the next commit yields 3.
    let r3 = forker
        .call_committing(&ZERO, COUNTER_ADDR.as_slice(), &[], U256::ZERO)
        .unwrap();
    assert_eq!(read_u256(&r3.result), U256::from(3));

    // The trace arena recorded the call to the counter contract.
    let traces = r3.traces.expect("traces present");
    let node = &traces.nodes()[0];
    assert_eq!(Address::from(node.trace.address.into_array()), COUNTER_ADDR);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_alloy_call_typed() {
    let (_anvil, url) = anvil_with_code(CONST_ADDR, const42_runtime()).await;
    let forker = Forker::new_with_fork(
        NewForkedEvm {
            fork_url: url,
            fork_block_number: None,
        },
        None,
        None,
    )
    .await
    .unwrap();

    // The contract returns 42 for any call; the typed return decodes to uint256.
    let res = match forker
        .alloy_call(Address::ZERO, CONST_ADDR, Demo::getCall {}, false)
        .await
    {
        Ok(r) => r,
        Err(e) => panic!("alloy_call failed: {e:?}"),
    };
    assert!(res.raw.exit_reason.is_ok());
    assert_eq!(res.typed_return, U256::from(42));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_alloy_call_revert_surfaces_failed() {
    let (_anvil, url) = anvil_with_code(REVERT_ADDR, revert_runtime()).await;
    let forker = Forker::new_with_fork(
        NewForkedEvm {
            fork_url: url,
            fork_block_number: None,
        },
        None,
        None,
    )
    .await
    .unwrap();

    // A reverting call with decoding off surfaces as `Failed` carrying the raw result.
    let res = forker
        .alloy_call(Address::ZERO, REVERT_ADDR, Demo::getCall {}, false)
        .await;
    assert!(matches!(res, Err(ForkCallError::Failed(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_add_or_select_then_call_and_roll() {
    let anvil = Anvil::new().spawn();
    let mut forker = Forker::new().unwrap();

    // add_or_select populates an empty forker.
    forker
        .add_or_select(
            NewForkedEvm {
                fork_url: anvil.endpoint(),
                fork_block_number: None,
            },
            None,
        )
        .await
        .unwrap();

    let input = [9u8; 16];
    let res = forker.call(&ZERO, &identity_precompile(), &input).unwrap();
    assert_eq!(res.result.as_ref(), &input);

    // roll_fork on an active fork succeeds.
    forker.roll_fork(Some(0), None).unwrap();
    let res = forker.call(&ZERO, &identity_precompile(), &input).unwrap();
    assert_eq!(res.result.as_ref(), &input);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_replay_transaction() {
    let anvil = Anvil::new().spawn();
    let provider = ProviderBuilder::new().connect_http(anvil.endpoint().parse().unwrap());
    provider
        .raw_request::<_, ()>("anvil_setCode".into(), (COUNTER_ADDR, counter_runtime()))
        .await
        .unwrap();

    // Send a transaction to the counter (increments 0 -> 1), mined by anvil.
    let from = anvil.addresses()[0];
    let tx = TransactionRequest::default().from(from).to(COUNTER_ADDR);
    let receipt = provider
        .send_transaction(tx)
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    let tx_hash = receipt.transaction_hash;

    let mut forker = Forker::new_with_fork(
        NewForkedEvm {
            fork_url: anvil.endpoint(),
            fork_block_number: None,
        },
        None,
        None,
    )
    .await
    .unwrap();

    // Replaying re-executes the tx in its block context: counter was 0, becomes 1.
    let res = forker.replay_transaction(tx_hash).await.unwrap();
    assert_eq!(U256::from_be_slice(&res.result), U256::from(1));
}

/// Runtime that halts immediately (STOP), returning empty data on success.
fn stop_runtime() -> alloy::primitives::Bytes {
    bytes!("00")
}

const STOP_ADDR: Address = address!("00000000000000000000000000000000000000dd");

#[test]
fn test_forkid_new() {
    let a = rain_forker::ForkId::new("https://example.com", Some(7));
    let b = rain_forker::ForkId::new("https://example.com", Some(7));
    let c = rain_forker::ForkId::new("https://example.com", None);
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn test_default_forker_has_no_active_fork() {
    let forker = Forker::default();
    let result = forker.call(&ZERO, &identity_precompile(), &[]);
    assert!(
        matches!(result, Err(ForkCallError::ExecutorError(ref msg)) if msg == "no active fork!")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_alloy_call_committing_typed() {
    let (_anvil, url) = anvil_with_code(COUNTER_ADDR, counter_runtime()).await;
    let mut forker = Forker::new_with_fork(
        NewForkedEvm {
            fork_url: url,
            fork_block_number: None,
        },
        None,
        None,
    )
    .await
    .unwrap();

    // The counter ignores calldata, so a typed committing call increments and
    // returns the new value, persisting across calls.
    let first = match forker
        .alloy_call_committing(
            Address::ZERO,
            COUNTER_ADDR,
            Demo::getCall {},
            U256::ZERO,
            false,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => panic!("alloy_call_committing failed: {e:?}"),
    };
    assert_eq!(first.typed_return, U256::from(1));

    let second = forker
        .alloy_call_committing(
            Address::ZERO,
            COUNTER_ADDR,
            Demo::getCall {},
            U256::ZERO,
            false,
        )
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(second.typed_return, U256::from(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_typed_error_on_undecodable_return() {
    let (_anvil, url) = anvil_with_code(STOP_ADDR, stop_runtime()).await;
    let forker = Forker::new_with_fork(
        NewForkedEvm {
            fork_url: url,
            fork_block_number: None,
        },
        None,
        None,
    )
    .await
    .unwrap();

    // The call succeeds with empty output, which cannot decode to uint256.
    let res = forker
        .alloy_call(Address::ZERO, STOP_ADDR, Demo::getCall {}, false)
        .await;
    assert!(matches!(res, Err(ForkCallError::TypedError(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_pinned_fork_reads_historical_state() {
    let anvil = Anvil::new().spawn();
    let provider = ProviderBuilder::new().connect_http(anvil.endpoint().parse().unwrap());
    provider
        .raw_request::<_, ()>("anvil_setCode".into(), (COUNTER_ADDR, counter_runtime()))
        .await
        .unwrap();

    // A tx in block 1 increments the counter 0 -> 1.
    let from = anvil.addresses()[0];
    let receipt = provider
        .send_transaction(TransactionRequest::default().from(from).to(COUNTER_ADDR))
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    let block1 = receipt.block_number.unwrap();

    // Forking pinned at block 1 sees the stored value 1, so a read computes 1 + 1.
    let forker = Forker::new_with_fork(
        NewForkedEvm {
            fork_url: anvil.endpoint(),
            fork_block_number: Some(block1),
        },
        None,
        None,
    )
    .await
    .unwrap();
    let res = forker.call(&ZERO, COUNTER_ADDR.as_slice(), &[]).unwrap();
    assert_eq!(U256::from_be_slice(&res.result), U256::from(2));
}
