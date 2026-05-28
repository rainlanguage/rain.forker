//! Integration tests for the `Forker`.
//!
//! The no-network tests exercise the error paths directly. The remaining tests
//! spin up a local `anvil` node (via alloy's node bindings) and fork it, so they
//! require `anvil` on PATH (provided by the rainix dev shell).

use alloy::node_bindings::Anvil;
use alloy::primitives::{address, bytes, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use rain_forker::{ForkCallError, Forker, NewForkedEvm};

const ZERO: [u8; 20] = [0u8; 20];

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
