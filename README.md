# exchange-core-rs

**English** · [中文](README_cn.md)

![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)
![edition](https://img.shields.io/badge/edition-2021-blue)
![status](https://img.shields.io/badge/status-alpha-yellow)

A deterministic, single-threaded matching & risk engine in Rust — a port of the core engine from
[**linking12/raft-exchange**](https://github.com/linking12/raft-exchange), a Raft-replicated crypto
exchange (Spot · Perpetual/Delivery Futures · Margin · Lending · UTA). It is designed to run as the
state machine behind a Raft consensus layer: every node applies the same ordered command stream and
reaches byte-for-byte identical state.

> `raft-exchange` (Java) combines JRaft consensus with a Disruptor execution pipeline on top of the
> classic exchange-core architecture. This crate reimplements that engine in Rust and **collapses the
> multi-processor Disruptor pipeline into one single-threaded deterministic pipeline**, so it can be
> driven directly by a Raft log.

---

## Features

- **Spot + Derivatives** — spot exchange, perpetual & delivery futures, isolated/cross margin, lending
  pools, funding fees, liquidation / ADL / insurance fund, delivery settlement.
- **Deterministic by construction** — one thread, one ordered pipeline (`R1 → ME → R2 → drain`). All
  output-affecting iteration uses `BTreeMap` / explicit ordering; **no `HashMap` iteration order**.
- **Fixed-point money** — amounts are `i64`, intermediate math is `i128` to prevent overflow.
- **Raft-ready** — pluggable command submitter and snapshot (`persist` / `recover`) hooks for
  install-snapshot; no embedded threading or transport.
- **Zero-cost logging** — `log` facade only; with no backend installed, `trace!`/`debug!` short-circuit.

---

## Installation

This crate is not yet published to crates.io; depend on it by path or git.

```toml
[dependencies]
exchange-core-rs = { git = "https://github.com/linking12/raft-exchange" }
# or
exchange-core-rs = { path = "../exchange-core-rs" }
```

MSRV: Rust 1.75 (edition 2021).

---

## Quick Start

`ExchangeApi` is a synchronous facade over an `ExchangeCore`. Configure it, submit commands, and read
the result code back from each call — no handlers required to get started.

```rust
use exchange_core_rs::core::exchange_api::{ExchangeApi, PlaceOrderRequest};
use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
use exchange_core_rs::core::common::symbol_type::SymbolType;
use exchange_core_rs::core::common::order_action::OrderAction;
use exchange_core_rs::core::common::order_type::OrderType;
use exchange_core_rs::core::common::cmd::command_result_code::CommandResultCode;

let mut api = ExchangeApi::new();

// Configure: two currencies, one spot symbol, two funded users.
api.add_currency(1, 1);
api.add_currency(2, 1);
api.add_symbol(CoreSymbolSpecification {
    symbol_id: 100, symbol_type: SymbolType::CurrencyExchangePair,
    base_currency: 1, quote_currency: 2, base_scale_k: 1, quote_scale_k: 1,
    taker_fee: 2, maker_fee: 1, ..Default::default()
});
api.add_user(1);
api.add_user(2);
api.balance_adjustment(1, 1, 1_000_000, 1);   // (uid, currency, amount, txid)
api.balance_adjustment(2, 2, 10_000_000, 2);

// Place a resting ask, then a crossing bid. Each call returns a CommandResultCode.
let r1 = api.place_order(PlaceOrderRequest { order_id: 5001, uid: 1, symbol: 100, price: 20_000, size: 1,
    reserve_bid_price: 0, action: OrderAction::Ask, order_type: OrderType::Gtc });
let r2 = api.place_order(PlaceOrderRequest { order_id: 5002, uid: 2, symbol: 100, price: 20_000, size: 1,
    reserve_bid_price: 20_000, action: OrderAction::Bid, order_type: OrderType::Gtc }); // a bid must set reserve_bid_price
assert_eq!(r1, CommandResultCode::Success);
assert_eq!(r2, CommandResultCode::Success); // the two orders matched

// Total balance across all accounts is always net-zero — a hard invariant.
assert!(api.total_balance().is_global_zero());
```

The API groups methods by domain (spot / futures / loan) and layer (configure → trade → query →
report). Futures and lending have dedicated request types (`PlaceFuturesOrderRequest`,
`ClosePositionRequest`, `loan_create`, `pool_deposit`, …); less common commands go through the generic
`api.submit(OrderCommand)` entry point.

### Receiving events

To observe fills, order-book snapshots, and fund movements (balances, PnL, fees), implement the two
handler traits and attach them via `api.core()` before trading. Reports are dispatched synchronously as
each command is applied — e.g. a Raft server forwards them to Kafka.

```rust
use exchange_core_rs::core::trade_events_handler::{
    TradeEventsHandler, OrderBook, SpotExecutionReport, FuturesExecutionReport,
};
use exchange_core_rs::core::fund_events_handler::{FundEventsHandler, FundEventReport};

struct MyTradeHandler;
impl TradeEventsHandler for MyTradeHandler {
    fn order_book(&mut self, _ob: OrderBook) {}
    fn spot_execution_report(&mut self, _r: SpotExecutionReport) { /* a spot fill/reject */ }
    fn futures_execution_report(&mut self, _r: FuturesExecutionReport) { /* a futures fill/reject */ }
}
struct MyFundHandler;
impl FundEventsHandler for MyFundHandler {
    fn fund_event_report(&mut self, _r: FundEventReport) { /* balance / PnL / fee movement */ }
}

api.core().with_events_handlers(MyTradeHandler, MyFundHandler);
```

---

## Raft integration

To run as a Raft state machine, wire up three things through `api.core()`: a command submitter for
cascaded commands, the apply loop, and snapshots.

```rust
use std::{cell::RefCell, collections::VecDeque, rc::Rc};
use exchange_core_rs::core::common::cmd::order_command::OrderCommand;
use exchange_core_rs::core::processors::liquidation::command_submitter::CommandSubmitter;

// 1. Command submitter. Cascaded commands (FORCE-liquidation → insurance-fund takeover → ADL,
//    loan liquidation) are proposed to Raft instead of applied in place. Here we collect them into a
//    queue; in a cluster, submit() would call raft.propose(cmd).
struct RaftSubmitter { queue: Rc<RefCell<VecDeque<OrderCommand>>> }
impl CommandSubmitter for RaftSubmitter {
    fn submit(&mut self, cmd: OrderCommand) { self.queue.borrow_mut().push_back(cmd); }
}
let queue: Rc<RefCell<VecDeque<OrderCommand>>> = Rc::new(RefCell::new(VecDeque::new()));
api.core().with_command_submitter(Rc::new(RefCell::new(RaftSubmitter { queue: queue.clone() })));

// 2. Apply loop. Feed each committed command to the engine, then drain the cascaded commands it
//    produced back through the same path until the queue is empty.
api.submit(committed_cmd);
loop {
    let next = queue.borrow_mut().pop_front(); // pop first, releasing the borrow before re-entering submit
    let Some(cmd) = next else { break };
    api.submit(cmd);
}

// 3a. Periodic liquidation scan. The leader ticks its clock; the tick proposes a LIQUIDATION_SCAN
//     command to Raft, so every node runs the scan deterministically on apply.
api.core().tick_liquidation_scheduler(now_ms);

// 3b. Snapshots for Raft install-snapshot. persist() writes the RE/ME/EC modules for snapshot_id;
//     a joining or restarting node calls recover() to load them.
api.core().persist(snapshot_id, instance_id);
api.core().recover(snapshot_id, instance_id);
```

Snapshots use **Chronicle Wire** binary framing (interoperable with the Java `.ecs`/`.dat` snapshots,
RE/ME modules + LZ4 autodetect). A separate `EC` module stores Rust-side counters (e.g. the result
sequence) that the Java side does not persist; it is read optionally, so Java/legacy snapshots recover
cleanly.

---

## Architecture

### Deterministic pipeline

Every command flows through one synchronous pipeline. Liquidation is not a background thread: mark-price
updates or `LIQUIDATION_SCAN` run risk checks in R1, enqueue liquidation commands via the *command
submitter*, and `drive_pending` replays them through the same pipeline until the queue drains — all
within a single `process_command` call.

```text
process_command(cmd):
    apply_one(cmd):
        R1  risk.pre_process_command    // validate / freeze / position pre-check / scan
        ME  matching.process_order      // match (place/cancel/move/reduce/liquidation-taker)
        R2  risk.handler_risk_release   // settle / release / PnL / fees / emit events
        └─ results_consumer(cmd, seq…)  // dispatch results & events downstream (if attached)
    drive_pending():                    // replay cascaded FORCE→IF→ADL / loan-liquidation commands
                                        // until the pending queue is empty
```

### Module layout

`raft-exchange`'s engine is a single Java module, so the Rust side is a **single crate**, split by
domain under `src/core/`.

| Module | Responsibility |
|--------|----------------|
| `core::common` | Domain model — `OrderCommand`, `CoreSymbolSpecification`, `UserProfile`, `Order`, `FundEvent`, … |
| `core::common::cmd` | Command types `OrderCommandType` + result codes `CommandResultCode` |
| `core::orderbook` | Order book: `IOrderBook` + Direct (O(log N)) / Naive impls |
| `core::processors` | `RiskEngine`, `MatchingEngineRouter`, funding fees, ADL |
| `core::processors::loan` | Lending: dispatch, liquidation engine, rate curves |
| `core::processors::liquidation` | Liquidation engine, insolvency price, scan scheduler |
| `core::exchange_core` | `ExchangeCore` orchestration (pipeline + snapshot) |
| `core::exchange_api` | `ExchangeApi` high-level facade |
| `core::reports` | Reports: global balance conservation, per-user, insurance fund |
| `core::utils` | Fixed-point arithmetic (`i128` intermediates, scaling) |

---

## Building & Testing

```bash
cargo build --release

cargo test --lib                 # unit tests co-located with production code
cargo test --test e2e            # engine-level e2e + conservation proptests
cargo test --test integration    # Java-oracle parity tests (public API only)
cargo test --test conformance    # golden-vector parity (generate golden via Java first)
cargo test --test orderbook_diff # Direct vs Naive order book differential
cargo test                       # everything

cargo bench --bench engine_throughput  # raw engine throughput (bypasses Raft)
```

Test layout: `src/` keeps only unit tests co-located with production code; standalone tests live under
`tests/` and use the public API only.

---

## Relationship to raft-exchange

This crate ports the **matching and risk engine** of the Java project. It deliberately does **not**
reimplement the Disruptor async submission layer, JRaft/Aeron transport, or `groupingControl` — those
belong to the surrounding server. The engine is consumed as a pure library and driven by a Raft state
machine.

Behavioral equivalence with the Java implementation is protected by a layered strategy (IT translation
parity / conservation proptests / golden-vector parity against a Java oracle) plus differential fuzzing.
See [`CONSISTENCY.md`](CONSISTENCY.md).

---

## License

Not yet specified in this repository. Licensing follows the upstream
[linking12/raft-exchange](https://github.com/linking12/raft-exchange) project — add a `LICENSE` file
before distribution.
