use exchange_core_rs::core::exchange_api::ExchangeApi;
use exchange_core_rs::core::processors::parallel::ComputeConfig;

/// build: sets up initial state (add currencies / symbols, open accounts, fund, seed orders).
/// run: executes the command under test (funding / scan / adl / etc).
/// Asserts that the resulting `state_hash` is identical whether the engine runs with
/// `workers=1` (serial) or `workers=8` (parallel), for two independently-built engines.
pub fn assert_workers_equivalent(
    build: impl Fn(&mut ExchangeApi),
    run: impl Fn(&mut ExchangeApi),
) {
    let mut a = ExchangeApi::new();
    a.with_compute_pool(ComputeConfig { workers: 1, serial_threshold: 1024 });
    build(&mut a);
    run(&mut a);

    let mut b = ExchangeApi::new();
    b.with_compute_pool(ComputeConfig { workers: 8, serial_threshold: 0 });
    build(&mut b);
    run(&mut b);

    assert_eq!(a.state_hash(), b.state_hash(), "workers=1 vs workers=8 state_hash must match");
}

#[test]
fn sanity_spot_match_is_worker_invariant() {
    use exchange_core_rs::core::exchange_api::PlaceOrderRequest;
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;

    assert_workers_equivalent(
        |api| {
            api.add_currency(1, 1);
            api.add_currency(2, 1);
            api.add_symbol(CoreSymbolSpecification {
                symbol_id: 100, symbol_type: SymbolType::CurrencyExchangePair,
                base_currency: 1, quote_currency: 2, base_scale_k: 1, quote_scale_k: 1,
                ..Default::default()
            });
            for uid in 1..=50i64 { api.add_user(uid); api.balance_adjustment(uid, 2, 10_000_000, uid); }
        },
        |api| {
            for uid in 1..=50i64 {
                api.place_order(PlaceOrderRequest { order_id: uid, uid, symbol: 100, price: 20_000, size: 1,
                    reserve_bid_price: 20_000, action: OrderAction::Bid, order_type: OrderType::Gtc });
            }
        },
    );
}

/// Task 2.1: funding-fee `collect_input` scan parallelized via `TwoStepCommandProcessor::map_users`.
/// Builds a perpetual-futures symbol with 2000 users (1000 long/short pairs, well past the
/// serial_threshold=1024 default so workers=8 actually forks), opens matched long/short
/// positions, then settles funding fees. Longs pay, shorts receive; asserts the resulting
/// `state_hash` (and thus the per-user payer/receiver shard + pro-rata remainder distribution)
/// is identical whether the scan ran serially (workers=1) or in parallel (workers=8).
#[test]
fn funding_settlement_is_worker_invariant() {
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::margin_mode::MarginMode;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::exchange_api::PlaceFuturesOrderRequest;

    const BASE: i32 = 1;
    const QUOTE: i32 = 2;
    const FUT_SYMBOL: i32 = 900;
    const NUM_PAIRS: i64 = 1000; // 2000 users total, > serial_threshold(1024)

    assert_workers_equivalent(
        |api| {
            api.add_currency(BASE, 1);
            api.add_currency(QUOTE, 1);
            api.add_futures_symbol(CoreSymbolSpecification {
                symbol_id: FUT_SYMBOL,
                symbol_type: SymbolType::FuturesContractPerpetual,
                base_currency: BASE,
                quote_currency: QUOTE,
                base_scale_k: 1,
                quote_scale_k: 1,
                ..Default::default()
            });
            api.set_mark_price(FUT_SYMBOL, 100);

            let mut order_id: i64 = 1;
            for pair in 0..NUM_PAIRS {
                let short_uid = pair * 2 + 1;
                let long_uid = pair * 2 + 2;
                api.add_user(short_uid);
                api.add_user(long_uid);
                api.balance_adjustment(short_uid, QUOTE, 1_000_000, short_uid);
                api.balance_adjustment(long_uid, QUOTE, 1_000_000, long_uid);

                api.place_futures_order(PlaceFuturesOrderRequest {
                    order_id,
                    uid: short_uid,
                    symbol: FUT_SYMBOL,
                    price: 100,
                    size: 10,
                    action: OrderAction::Ask,
                    order_type: OrderType::Gtc,
                    leverage: 1,
                    margin_mode: MarginMode::Isolated,
                    reduce_only: false,
                });
                order_id += 1;

                api.place_futures_order(PlaceFuturesOrderRequest {
                    order_id,
                    uid: long_uid,
                    symbol: FUT_SYMBOL,
                    price: 100,
                    size: 10,
                    action: OrderAction::Bid,
                    order_type: OrderType::Gtc,
                    leverage: 1,
                    margin_mode: MarginMode::Isolated,
                    reduce_only: false,
                });
                order_id += 1;
            }
        },
        |api| {
            // action=Bid -> longs are the payer side (Self::collect_input's payer_dir),
            // shorts are the receiver side. rate=1, rate_scale_k=1000 -> small nonzero fee
            // per payer so build_matcher_events / apply_event actually move money.
            api.settle_funding_fees(FUT_SYMBOL, OrderAction::Bid, 1, 1000, 999_999);
        },
    );
}

/// Task 3.1: futures liquidation `check_positions` decision phase parallelized via
/// `ComputePool::map` (`LiquidationEngine::decide_user` / `apply_scan_outcome`).
/// Builds a perpetual-futures symbol with 2000 position-holders (1000 long/short pairs, past
/// the serial_threshold=1024 default so workers=8 actually forks): pairs alternate between a
/// thin (leverage=20) and a safe (leverage=2) risk profile. A single mark-price drop then
/// triggers a TARGETED scan (`MarkpriceAdjustment` -> `RiskEngineCommandDispatcher::markprice_adjustment`
/// -> `LiquidationEngine::check_positions`) over all 2000 holders, producing a real mix of
/// outcomes: thin longs go underwater and liquidate, safe longs and all shorts (which profit
/// from the drop) stay healthy. Asserts the resulting `state_hash` — and thus the per-uid
/// decide/apply order (MarginAlert/cross-alerts then LiquidationAlert, uids ascending) plus
/// every FORCE liquidation command it queues — is identical whether the scan ran serially
/// (workers=1) or in parallel (workers=8).
#[test]
fn futures_scan_is_worker_invariant() {
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::margin_mode::MarginMode;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::exchange_api::PlaceFuturesOrderRequest;
    use std::collections::BTreeMap;

    const BASE: i32 = 1;
    const QUOTE: i32 = 2;
    const FUT_SYMBOL: i32 = 901;
    const NUM_PAIRS: i64 = 1000; // 2000 users total, > serial_threshold(1024)
    const ENTRY: i64 = 10_000;
    const DROP: i64 = 9_000; // 10% drop: leverage=20 longs go underwater; leverage=2 longs + all shorts stay healthy

    fn futures_spec() -> CoreSymbolSpecification {
        let mut mm = BTreeMap::new();
        mm.insert(1_000i64, 5i64);
        mm.insert(100_000i64, 10i64);
        CoreSymbolSpecification {
            symbol_id: FUT_SYMBOL,
            symbol_type: SymbolType::FuturesContractPerpetual,
            base_currency: BASE,
            quote_currency: QUOTE,
            base_scale_k: 1,
            quote_scale_k: 1,
            maintenance_margin: mm,
            maintenance_margin_scale_k: 1_000,
            ..Default::default()
        }
    }

    assert_workers_equivalent(
        |api| {
            api.add_currency(BASE, 1);
            api.add_currency(QUOTE, 1);
            api.add_futures_symbol(futures_spec());
            api.set_mark_price(FUT_SYMBOL, ENTRY);
            api.enable_liquidation();

            let mut order_id: i64 = 1;
            for pair in 0..NUM_PAIRS {
                let short_uid = pair * 2 + 1;
                let long_uid = pair * 2 + 2;
                // Alternate risk profile per pair: thin (leverage=20) vs safe (leverage=2).
                let leverage: i32 = if pair % 2 == 0 { 20 } else { 2 };
                api.add_user(short_uid);
                api.add_user(long_uid);
                api.balance_adjustment(short_uid, QUOTE, 1_000_000, short_uid);
                api.balance_adjustment(long_uid, QUOTE, 1_000_000, long_uid);

                api.place_futures_order(PlaceFuturesOrderRequest {
                    order_id,
                    uid: short_uid,
                    symbol: FUT_SYMBOL,
                    price: ENTRY,
                    size: 1,
                    action: OrderAction::Ask,
                    order_type: OrderType::Gtc,
                    leverage,
                    margin_mode: MarginMode::Isolated,
                    reduce_only: false,
                });
                order_id += 1;

                api.place_futures_order(PlaceFuturesOrderRequest {
                    order_id,
                    uid: long_uid,
                    symbol: FUT_SYMBOL,
                    price: ENTRY,
                    size: 1,
                    action: OrderAction::Bid,
                    order_type: OrderType::Gtc,
                    leverage,
                    margin_mode: MarginMode::Isolated,
                    reduce_only: false,
                });
                order_id += 1;
            }
        },
        |api| {
            api.set_mark_price(FUT_SYMBOL, DROP);
        },
    );
}

/// Task 4.1: ADL profitable-position scan parallelized via `TwoStepCommandProcessor::map_users`
/// (`LiquidationService::profit_one`), with ranking (`AdlCommandProcessor::collect_input`) made a
/// deterministic total order: `(risk_score desc, uid desc)`.
///
/// Builds a perpetual-futures symbol with one taker (LONG) and 1200 SHORT counterparties, all
/// opened at the identical price/size/leverage against the taker's single resting bid, so every
/// counterparty's `risk_score` ties exactly (same actual_leverage, same unrealized_pnl, same
/// default ISOLATED `adl_eligibility`). A mark-price drop makes every short profitable, then an
/// AUTO_DELEVERAGING command requests exactly half the total size -- so which half gets picked is
/// driven entirely by the tie-break, not by score. Asserts the resulting `state_hash` is identical
/// whether the scan ran serially (workers=1) or in parallel (workers=8).
///
/// `assert_workers_equivalent` only compares `state_hash`, so separately (outside it, on a plain
/// `ExchangeApi`) this also asserts the run was non-vacuous and picked the expected half: the
/// highest-uid tied candidate (ranked first under `(score, uid desc)`) had its 1-lot position fully
/// closed, while the lowest-uid tied candidate (budget exhausted before reaching it) was left with
/// its position and `pending_adl_size` untouched.
#[test]
fn adl_is_worker_invariant() {
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::margin_mode::MarginMode;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::exchange_api::{AutoDeleveragingRequest, PlaceFuturesOrderRequest};

    const BASE: i32 = 1;
    const QUOTE: i32 = 2;
    const FUT_SYMBOL: i32 = 902;
    const TAKER: i64 = 1;
    const CP_FIRST: i64 = 100;
    const NUM_CP: i64 = 1200; // > serial_threshold(1024): the workers=8 config actually forks
    const CP_LAST: i64 = CP_FIRST + NUM_CP - 1; // 1299
    const ENTRY: i64 = 10_000;
    const DROP: i64 = 9_000; // below ENTRY -> every SHORT counterparty is profitable
    const LEVERAGE: i32 = 5;
    const ADL_SIZE: i64 = NUM_CP / 2; // 600: a partial pick, driven entirely by the uid tie-break

    fn build(api: &mut ExchangeApi) {
        api.add_currency(BASE, 1);
        api.add_currency(QUOTE, 1);
        api.add_futures_symbol(CoreSymbolSpecification {
            symbol_id: FUT_SYMBOL,
            symbol_type: SymbolType::FuturesContractPerpetual,
            base_currency: BASE,
            quote_currency: QUOTE,
            base_scale_k: 1,
            quote_scale_k: 1,
            ..Default::default()
        });
        api.set_mark_price(FUT_SYMBOL, ENTRY);

        api.add_user(TAKER);
        api.balance_adjustment(TAKER, QUOTE, 1_000_000_000, TAKER);
        api.place_futures_order(PlaceFuturesOrderRequest {
            order_id: 1,
            uid: TAKER,
            symbol: FUT_SYMBOL,
            price: ENTRY,
            size: NUM_CP,
            action: OrderAction::Bid,
            order_type: OrderType::Gtc,
            leverage: LEVERAGE,
            margin_mode: MarginMode::Isolated,
            reduce_only: false,
        });

        let mut order_id: i64 = 2;
        for uid in CP_FIRST..=CP_LAST {
            api.add_user(uid);
            api.balance_adjustment(uid, QUOTE, 1_000_000, uid);
            api.place_futures_order(PlaceFuturesOrderRequest {
                order_id,
                uid,
                symbol: FUT_SYMBOL,
                price: ENTRY,
                size: 1,
                action: OrderAction::Ask,
                order_type: OrderType::Gtc,
                leverage: LEVERAGE,
                margin_mode: MarginMode::Isolated,
                reduce_only: false,
            });
            order_id += 1;
        }

        api.set_mark_price(FUT_SYMBOL, DROP);
    }

    fn run(api: &mut ExchangeApi) {
        api.submit_auto_deleveraging(AutoDeleveragingRequest {
            order_id: 999_999,
            uid: TAKER,
            symbol: FUT_SYMBOL,
            action: OrderAction::Bid,
            size: ADL_SIZE,
            price: DROP,
            timestamp: 1,
        });
    }

    assert_workers_equivalent(build, run);

    let mut check = ExchangeApi::new();
    build(&mut check);
    run(&mut check);
    assert!(
        check.user_position(CP_LAST, FUT_SYMBOL).is_none(),
        "highest-uid tied candidate must be picked first under the (score, uid desc) tie-break and fully closed (size=1 == available)"
    );
    let untouched = check
        .user_position(CP_FIRST, FUT_SYMBOL)
        .expect("lowest-uid tied candidate must be left untouched: budget exhausted before reaching it");
    assert_eq!(untouched.open_volume, 1, "not selected -> position must be untouched");
    assert_eq!(untouched.pending_adl_size, 0, "not selected -> pending_adl_size must remain 0");
}

/// Task 5.1: LOAN liquidation `check_loans` decision phase parallelized via `ComputePool::map`
/// (`LoanLiquidationEngine::decide_loans` / `apply_loan_outcome`) -- the highest-risk of the four
/// scans, since the old serial code interleaved decision with command submission inside
/// `check_isolated`/`check_cross`.
///
/// Builds one isolated-loan spot symbol (ETH/XBT), a lending pool funded in XBT, and 1200
/// isolated-loan borrowers (past the default `serial_threshold`=1024, so `workers=8` actually
/// forks) split into 3 equal risk profiles by `uid % 3`, all opened at the same collateral/price
/// so LTV is driven purely by principal:
/// - group 0 ("breach"): principal so post-crash LTV ~= 100%, well over the 80% liquidation
///   threshold -> queues a `LoanForceLiquidate` command.
/// - group 1 ("margin call"): principal so post-crash LTV ~= 72%, between the 70% margin-call
///   and 80% liquidation thresholds -> queues a `LoanMarginCall` alert only, no command.
/// - group 2 ("healthy"): principal so post-crash LTV ~= 40%, under every threshold -> nothing.
///
/// A single LP resting GTC bid absorbs every `LoanForceLiquidate` IOC sell. A targeted
/// `MarkpriceAdjustment` (via `set_mark_price`) on the loan symbol then triggers `check_loans`'
/// targeted branch (union of `isolated_loan_symbol_to_users`) over all 1200 holders, exercising
/// both the parallel decide phase (LTV/threshold math, building the force-liquidate command) and
/// the serial uid-ascending apply phase (alert-then-submit, byte-identical order to the old
/// interleaved code). Asserts the resulting `state_hash` -- and thus every submitted
/// `LoanForceLiquidate` command plus every `LoanMarginCall` alert, in uid-ascending order -- is
/// identical whether the scan ran serially (workers=1) or in parallel (workers=8).
#[test]
fn loan_scan_is_worker_invariant() {
    use exchange_core_rs::core::common::cmd::order_command::OrderCommand;
    use exchange_core_rs::core::common::cmd::order_command_type::OrderCommandType;
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::fund_event::FundEventType;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::exchange_api::PlaceOrderRequest;

    const ETH: i32 = 1;
    const XBT: i32 = 2;
    const SYMBOL: i32 = 903;
    const LP: i64 = 1;
    const BORROWER_FIRST: i64 = 10_002; // multiple of 3, so uid % 3 == (uid - BORROWER_FIRST) % 3 for every borrower
    const NUM_BORROWERS: i64 = 1200; // > serial_threshold(1024): the workers=8 config actually forks
    const OPEN_MARK: i64 = 1_000;
    const CRASH_MARK: i64 = 500; // 50% crash -> post-crash LTV ~= 2x pre-crash LTV
    const ETH_COLLATERAL: i64 = 100;
    const LOAN_TS: i64 = 1_000;
    // principal chosen so pre-crash LTV = principal / (ETH_COLLATERAL * OPEN_MARK):
    const PRINCIPAL_BREACH: i64 = 50_000; // pre=50% -> post~=100% (>= 80% liquidation_ltv_bps)
    const PRINCIPAL_MARGIN_CALL: i64 = 36_000; // pre=36% -> post~=72% (in [70%, 80%) -> alert only)
    const PRINCIPAL_HEALTHY: i64 = 20_000; // pre=20% -> post~=40% (< 70% margin_call_ltv_bps)
    const LP_ABSORB_SIZE: i64 = 60_000; // >> total breach-group sell volume (400 * 100 = 40,000)

    fn loan_spec() -> CoreSymbolSpecification {
        let mut spec = CoreSymbolSpecification {
            symbol_id: SYMBOL,
            symbol_type: SymbolType::CurrencyExchangePair,
            base_currency: ETH,
            quote_currency: XBT,
            base_scale_k: 1,
            quote_scale_k: 1,
            ..Default::default()
        };
        spec.loan_config.update(6_000, 8_000, 7_000, i64::MAX, 365);
        spec
    }

    fn principal_for(uid: i64) -> i64 {
        match uid % 3 {
            0 => PRINCIPAL_BREACH,
            1 => PRINCIPAL_MARGIN_CALL,
            _ => PRINCIPAL_HEALTHY,
        }
    }

    fn build(api: &mut ExchangeApi) {
        api.add_currency(ETH, 1);
        api.add_currency(XBT, 1);
        api.add_symbol(loan_spec());
        api.set_mark_price(SYMBOL, OPEN_MARK);

        api.submit(OrderCommand { command: OrderCommandType::PoolDeposit, order_id: 1, symbol: XBT, size: 100_000_000, ..Default::default() });

        api.add_user(LP);
        api.balance_adjustment(LP, XBT, LP_ABSORB_SIZE * CRASH_MARK * 4, LP);

        let mut order_id: i64 = 2;
        for i in 0..NUM_BORROWERS {
            let uid = BORROWER_FIRST + i;
            api.add_user(uid);
            api.balance_adjustment(uid, ETH, ETH_COLLATERAL, uid);
            api.submit(OrderCommand {
                command: OrderCommandType::LoanCreate,
                order_id,
                uid,
                symbol: SYMBOL,
                size: ETH_COLLATERAL,
                price: principal_for(uid),
                reserve_bid_price: uid, // loan_id = uid: unique per single-loan borrower
                timestamp: LOAN_TS,
                ..Default::default()
            });
            order_id += 1;
        }

        api.place_order(PlaceOrderRequest {
            order_id,
            uid: LP,
            symbol: SYMBOL,
            price: CRASH_MARK,
            size: LP_ABSORB_SIZE,
            reserve_bid_price: CRASH_MARK,
            action: OrderAction::Bid,
            order_type: OrderType::Gtc,
        });

        api.enable_liquidation();
    }

    fn run(api: &mut ExchangeApi) {
        api.set_mark_price(SYMBOL, CRASH_MARK);
    }

    assert_workers_equivalent(build, run);

    // Non-vacuous: separately re-run once and check every profile landed on its expected branch.
    let mut check = ExchangeApi::new();
    build(&mut check);
    run(&mut check);

    let breach_uid = BORROWER_FIRST; // uid % 3 == 0
    let margin_call_uid = BORROWER_FIRST + 1; // uid % 3 == 1
    let healthy_uid = BORROWER_FIRST + 2; // uid % 3 == 2

    let breach_collateral = check
        .ups()
        .get(breach_uid)
        .and_then(|up| up.isolated_loans.get(&breach_uid))
        .map(|l| l.collateral_amount)
        .unwrap_or(0);
    assert!(
        breach_collateral < ETH_COLLATERAL,
        "breach-group borrower's isolated loan must have been force-liquidated (collateral consumed by the IOC sell), got {breach_collateral}"
    );

    let healthy_collateral = check
        .ups()
        .get(healthy_uid)
        .and_then(|up| up.isolated_loans.get(&healthy_uid))
        .map(|l| l.collateral_amount)
        .unwrap_or(0);
    assert_eq!(healthy_collateral, ETH_COLLATERAL, "healthy-group borrower must be untouched by the scan");

    let margin_call_alert_fired = check
        .last_fund_events()
        .iter()
        .any(|e| e.uid == margin_call_uid && e.event_type == FundEventType::LoanMarginCall);
    assert!(margin_call_alert_fired, "margin-call-group borrower must have emitted a LoanMarginCall alert, not a liquidation");

    let margin_call_collateral = check
        .ups()
        .get(margin_call_uid)
        .and_then(|up| up.isolated_loans.get(&margin_call_uid))
        .map(|l| l.collateral_amount)
        .unwrap_or(0);
    assert_eq!(margin_call_collateral, ETH_COLLATERAL, "margin-call-group borrower must not be liquidated (alert only)");

    assert!(check.total_balance().is_global_zero(), "global balance must be conserved after the parallel loan scan");
}
