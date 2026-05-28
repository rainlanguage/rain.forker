# rain.forker

A generic forked-EVM executor for the Rain ecosystem.

`rain-forker` is a thin wrapper over [`revm`](https://crates.io/crates/revm) +
[`foundry-fork-db`](https://crates.io/crates/foundry-fork-db) that provides the
pieces previously supplied by `foundry-evm` — which is **not** published to
crates.io and so blocks `cargo publish` for any crate that depends on it:

- **Multi-fork RPC forking** — lazily fetch and cache real chain state over
  JSON-RPC (`new_with_fork`, `add_or_select`, `roll_fork`).
- **Typed alloy calls** — `alloy_call` / `alloy_call_committing` over any
  `SolCall`, plus raw `call` / `call_committing`.
- **Call tracing** — every call runs under a tracing inspector, exposing the
  `CallTraceArena` on the result.
- **Historical transaction replay** — `replay_transaction` re-executes every
  earlier transaction in a block, then the target, in its real context.

It was extracted from `rainlang-eval` specifically so that crate, and its
downstream consumers (e.g. raindex), can drop their last git dependencies and
publish to crates.io.

## Usage

```rust
use rain_forker::{Forker, NewForkedEvm};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let forker = Forker::new_with_fork(
    NewForkedEvm { fork_url: "https://...".into(), fork_block_number: None },
    None,
    None,
)
.await?;

let result = forker.call(&from, &to, &calldata)?;
assert!(result.exit_reason.is_ok());
# Ok(())
# }
```

## Development

Uses Nix flakes (`nix develop`) with the shared
[rainix](https://github.com/rainlanguage/rainix) toolchain.

```bash
nix develop -c cargo test    # unit + anvil-backed integration tests
nix develop -c cargo clippy
```

The integration tests spin up a local `anvil` node, provided by the rainix dev
shell.

## License

CAL-1.0
