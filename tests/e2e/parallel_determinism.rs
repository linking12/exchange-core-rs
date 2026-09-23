use proptest::prelude::*;

use exchange_core_rs::core::exchange_api::ExchangeApi;
use exchange_core_rs::core::processors::parallel::ComputeConfig;

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
    const NUM_PAIRS: i64 = 1000;
    const SHORT_UID_FIRST: i64 = 1;

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
    }

    fn run(api: &mut ExchangeApi) {
        api.settle_funding_fees(FUT_SYMBOL, OrderAction::Bid, 1, 1000, 999_999);
    }

    assert_workers_equivalent(build, run);

    // Non-vacuous: the parallel funding scan must have actually moved something --
    // while positions stay open the fee/rebate accrues into position.profit (not the
    // free account balance), so check that directly, and that a settlement event fired.
    let mut check = ExchangeApi::new();
    build(&mut check);
    let before_profit = check.user_position(SHORT_UID_FIRST, FUT_SYMBOL).expect("short position must exist before settlement").profit;
    run(&mut check);
    let after_profit = check.user_position(SHORT_UID_FIRST, FUT_SYMBOL).expect("short position must still exist after settlement").profit;
    assert_ne!(before_profit, after_profit, "funding settlement must move the short-side receiver's deferred position.profit");

    use exchange_core_rs::core::common::fund_event::FundEventType;
    assert!(
        check.last_fund_events().iter().any(|e| e.event_type == FundEventType::FundingfeeSettlement),
        "funding settlement must have produced at least one FundingfeeSettlement fund event"
    );
}

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
    const NUM_PAIRS: i64 = 1000;
    const ENTRY: i64 = 10_000;
    const DROP: i64 = 9_000;

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

    fn build(api: &mut ExchangeApi) {
        api.add_currency(BASE, 1);
        api.add_currency(QUOTE, 1);
        api.add_futures_symbol(futures_spec());
        api.set_mark_price(FUT_SYMBOL, ENTRY);
        api.enable_liquidation();

        let mut order_id: i64 = 1;
        for pair in 0..NUM_PAIRS {
            let short_uid = pair * 2 + 1;
            let long_uid = pair * 2 + 2;
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
    }

    fn run(api: &mut ExchangeApi) {
        api.set_mark_price(FUT_SYMBOL, DROP);
    }

    assert_workers_equivalent(build, run);

    // Non-vacuous: the parallel futures liquidation scan must have actually done
    // something (fired at least one liquidation-related fund event), not just left
    // both engines equally untouched.
    use exchange_core_rs::core::common::fund_event::FundEventType;
    let mut check = ExchangeApi::new();
    build(&mut check);
    run(&mut check);
    assert!(
        !check.last_fund_events().is_empty(),
        "futures scan must have produced at least one fund event from the price drop"
    );
    assert!(
        check.last_fund_events().iter().any(|e| matches!(
            e.event_type,
            FundEventType::LiquidationClose
                | FundEventType::LiquidationFee
                | FundEventType::IfPositionClose
                | FundEventType::MarginAlert
                | FundEventType::LiquidationAlert
        )),
        "futures scan must have produced at least one liquidation-related fund event, got: {:?}",
        check.last_fund_events().iter().map(|e| e.event_type).collect::<Vec<_>>()
    );
}

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
    const NUM_CP: i64 = 1200;
    const CP_LAST: i64 = CP_FIRST + NUM_CP - 1;
    const ENTRY: i64 = 10_000;
    const DROP: i64 = 9_000;
    const LEVERAGE: i32 = 5;
    const ADL_SIZE: i64 = NUM_CP / 2;

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

#[test]
fn loan_scan_is_worker_invariant() {
    use exchange_core_rs::core::common::cmd::order_command::OrderCommand;
    use exchange_core_rs::core::common::cmd::order_command_type::OrderCommandType;
    use exchange_core_rs::core::common::core_currency_specification::CoreCurrencySpecification;
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::fund_event::FundEventType;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::exchange_api::PlaceOrderRequest;

    const ETH: i32 = 1;
    const XBT: i32 = 2;
    // Separate currency + pair for the cross-loan cohort so its collateral (and LP
    // liquidity) is fully decoupled from the isolated cohort's ETH/XBT book.
    const ETH2: i32 = 3;
    const SYMBOL: i32 = 903;
    const SYMBOL_CROSS: i32 = 904;
    const LP: i64 = 1;
    const CROSS_LP: i64 = 2;
    const BORROWER_FIRST: i64 = 10_002;
    const NUM_BORROWERS: i64 = 1200;
    const CROSS_BORROWER_FIRST: i64 = 30_003; // 30_003 % 3 == 0, i.e. lands in the breach group below
    const NUM_CROSS_BORROWERS: i64 = 90;
    const OPEN_MARK: i64 = 1_000;
    const CRASH_MARK: i64 = 500;
    const ETH_COLLATERAL: i64 = 100;
    const CROSS_COLLATERAL: i64 = 100;
    const LOAN_TS: i64 = 1_000;
    const PRINCIPAL_BREACH: i64 = 50_000;
    const PRINCIPAL_MARGIN_CALL: i64 = 36_000;
    const PRINCIPAL_HEALTHY: i64 = 20_000;
    // Cross thresholds use the global defaults (liq=8500bps, margin_call=8000bps);
    // collateral_value at CRASH_MARK = 100 * 500 = 50_000.
    const CROSS_PRINCIPAL_BREACH: i64 = 45_000; // 90.0% > 85%
    const CROSS_PRINCIPAL_MARGIN_CALL: i64 = 41_000; // 82.0% in [80%,85%)
    const CROSS_PRINCIPAL_HEALTHY: i64 = 20_000; // 40.0% < 80%
    // Deliberately scarce relative to breach-group demand (~400 borrowers * 100 lots =
    // 40_000 lots): only the first 150 ascending-uid breach borrowers can fill, so the
    // final state depends on *processing order* -- if the parallel scan ever computed
    // or applied outcomes out of the canonical (sorted-uid) order, this would surface
    // as a workers=1 vs workers=8 state_hash divergence. Previously this was 60_000
    // (oversized: every breach borrower always fully liquidates, so order never
    // mattered and a reorder bug couldn't have been caught here).
    const LP_ABSORB_SIZE: i64 = 15_000;
    const CROSS_LP_ABSORB_SIZE: i64 = 10_000;

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

    fn cross_spot_spec() -> CoreSymbolSpecification {
        // LoanCrossBorrow requires `loan_config.is_enabled()` on this symbol (it's the
        // (selling_currency, loan_currency) pair looked up by handle_loan_cross_borrow),
        // even though the actual liquidation thresholds used by decide_cross come from
        // the global cross config, not this per-symbol one.
        let mut spec = CoreSymbolSpecification {
            symbol_id: SYMBOL_CROSS,
            symbol_type: SymbolType::CurrencyExchangePair,
            base_currency: ETH2,
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

    fn cross_principal_for(uid: i64) -> i64 {
        match uid % 3 {
            0 => CROSS_PRINCIPAL_BREACH,
            1 => CROSS_PRINCIPAL_MARGIN_CALL,
            _ => CROSS_PRINCIPAL_HEALTHY,
        }
    }

    fn build(api: &mut ExchangeApi) {
        api.add_currency(ETH, 1);
        api.add_currency(XBT, 1);
        api.add_currencies([CoreCurrencySpecification {
            currency: ETH2,
            currency_scale_k: 1,
            collateral_weight_bps: 10_000,
            ..Default::default()
        }]);
        api.add_symbol(loan_spec());
        api.add_symbol(cross_spot_spec());
        api.set_mark_price(SYMBOL, OPEN_MARK);
        api.set_mark_price(SYMBOL_CROSS, OPEN_MARK);

        api.submit(OrderCommand { command: OrderCommandType::PoolDeposit, order_id: 1, symbol: XBT, size: 100_000_000, ..Default::default() });

        api.core().risk.loan_service.global_config.numeraire_currency = XBT;

        api.add_user(LP);
        api.balance_adjustment(LP, XBT, LP_ABSORB_SIZE * CRASH_MARK * 4, LP);
        api.add_user(CROSS_LP);
        api.balance_adjustment(CROSS_LP, XBT, CROSS_LP_ABSORB_SIZE * CRASH_MARK * 4, CROSS_LP);

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
                reserve_bid_price: uid,
                timestamp: LOAN_TS,
                ..Default::default()
            });
            order_id += 1;
        }

        for i in 0..NUM_CROSS_BORROWERS {
            let uid = CROSS_BORROWER_FIRST + i;
            api.add_user(uid);
            api.balance_adjustment(uid, ETH2, CROSS_COLLATERAL, uid);
            api.loan_cross_add_collateral(order_id, uid, ETH2, CROSS_COLLATERAL, LOAN_TS);
            order_id += 1;
            api.loan_cross_borrow(order_id, uid, SYMBOL_CROSS, 1, cross_principal_for(uid), LOAN_TS);
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
        order_id += 1;

        api.place_order(PlaceOrderRequest {
            order_id,
            uid: CROSS_LP,
            symbol: SYMBOL_CROSS,
            price: CRASH_MARK,
            size: CROSS_LP_ABSORB_SIZE,
            reserve_bid_price: CRASH_MARK,
            action: OrderAction::Bid,
            order_type: OrderType::Gtc,
        });

        api.enable_liquidation();
    }

    fn run(api: &mut ExchangeApi) {
        // Cross first, isolated last -- so `last_fund_events()` (used below on a
        // dedicated `check` engine) reflects the isolated-group scan, matching the
        // isolated-group assertions; the cross-group alert is checked via its own
        // single-command engine further down.
        api.set_mark_price(SYMBOL_CROSS, CRASH_MARK);
        api.set_mark_price(SYMBOL, CRASH_MARK);
    }

    assert_workers_equivalent(build, run);

    let mut check = ExchangeApi::new();
    build(&mut check);
    run(&mut check);

    let breach_uid = BORROWER_FIRST;
    let margin_call_uid = BORROWER_FIRST + 1;
    let healthy_uid = BORROWER_FIRST + 2;

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

    // Cross-loan cohort: decide_cross must actually run under the parallel harness.
    let cross_breach_uid = CROSS_BORROWER_FIRST;
    let cross_margin_call_uid = CROSS_BORROWER_FIRST + 1;
    let cross_healthy_uid = CROSS_BORROWER_FIRST + 2;

    let cross_breach_collateral = check.ups().get(cross_breach_uid).map(|up| up.cross_loan_collateral(ETH2)).unwrap_or(0);
    assert!(
        cross_breach_collateral < CROSS_COLLATERAL,
        "breach-group cross borrower must have been force-liquidated (collateral consumed), got {cross_breach_collateral}"
    );

    let cross_healthy_collateral = check.ups().get(cross_healthy_uid).map(|up| up.cross_loan_collateral(ETH2)).unwrap_or(0);
    assert_eq!(cross_healthy_collateral, CROSS_COLLATERAL, "healthy-group cross borrower must be untouched by the scan");

    // The combined `run` fires SYMBOL_CROSS first, so its fund events are overwritten
    // by the later SYMBOL command; use a dedicated single-command engine to observe
    // the cross-group alert in isolation.
    let mut cross_check = ExchangeApi::new();
    build(&mut cross_check);
    cross_check.set_mark_price(SYMBOL_CROSS, CRASH_MARK);
    let cross_margin_call_alert_fired = cross_check
        .last_fund_events()
        .iter()
        .any(|e| e.uid == cross_margin_call_uid && e.event_type == FundEventType::LoanMarginCall && e.loan_mode == 1);
    assert!(cross_margin_call_alert_fired, "margin-call-group cross borrower must have emitted a cross (loan_mode=1) LoanMarginCall alert");

    assert!(check.total_balance().is_global_zero(), "global balance must be conserved after the parallel loan scan");
}

#[test]
fn cross_futures_liquidation_is_worker_invariant() {
    // The futures liquidation scan's CROSS branch (check_cross_decisions, grouped by
    // quote currency) is a distinct code path from the ISOLATED branch that
    // `futures_scan_is_worker_invariant` covers. Exercise it under both worker counts.
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::margin_mode::MarginMode;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::exchange_api::PlaceFuturesOrderRequest;
    use std::collections::BTreeMap;

    const BASE: i32 = 1;
    const QUOTE: i32 = 2;
    const FUT_SYMBOL: i32 = 905;
    const NUM_PAIRS: i64 = 300;
    const ENTRY: i64 = 10_000;
    const DROP: i64 = 5_000;
    const LEVERAGE: i32 = 20;
    // Long side is thinly funded (just over the init margin of ENTRY/LEVERAGE = 500), so
    // the crash to DROP drives its shared CROSS equity negative and it must liquidate;
    // the short side is deep, stays healthy, and rests as the counterparty.
    const THIN_BAL: i64 = 550;
    const DEEP_BAL: i64 = 1_000_000;

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

    fn build(api: &mut ExchangeApi) {
        api.add_currency(BASE, 1);
        api.add_currency(QUOTE, 1);
        api.add_futures_symbol(futures_spec());
        api.set_mark_price(FUT_SYMBOL, ENTRY);
        api.enable_liquidation();

        let mut order_id: i64 = 1;
        for pair in 0..NUM_PAIRS {
            let short_uid = pair * 2 + 1;
            let long_uid = pair * 2 + 2;
            api.add_user(short_uid);
            api.add_user(long_uid);
            api.balance_adjustment(short_uid, QUOTE, DEEP_BAL, short_uid);
            api.balance_adjustment(long_uid, QUOTE, THIN_BAL, long_uid);

            // short rests, long crosses -> both open a CROSS position at ENTRY.
            api.place_futures_order(PlaceFuturesOrderRequest {
                order_id, uid: short_uid, symbol: FUT_SYMBOL, price: ENTRY, size: 1,
                action: OrderAction::Ask, order_type: OrderType::Gtc, leverage: LEVERAGE,
                margin_mode: MarginMode::Cross, reduce_only: false,
            });
            order_id += 1;
            api.place_futures_order(PlaceFuturesOrderRequest {
                order_id, uid: long_uid, symbol: FUT_SYMBOL, price: ENTRY, size: 1,
                action: OrderAction::Bid, order_type: OrderType::Gtc, leverage: LEVERAGE,
                margin_mode: MarginMode::Cross, reduce_only: false,
            });
            order_id += 1;
        }
    }

    fn run(api: &mut ExchangeApi) {
        api.set_mark_price(FUT_SYMBOL, DROP);
    }

    assert_workers_equivalent(build, run);

    // Non-vacuous: the CROSS scan must actually have fired a liquidation-related event.
    use exchange_core_rs::core::common::fund_event::FundEventType;
    let mut check = ExchangeApi::new();
    build(&mut check);
    run(&mut check);
    assert!(
        check.last_fund_events().iter().any(|e| matches!(
            e.event_type,
            FundEventType::LiquidationClose
                | FundEventType::LiquidationFee
                | FundEventType::IfPositionClose
                | FundEventType::MarginAlert
                | FundEventType::LiquidationAlert
        )),
        "CROSS futures scan must produce at least one liquidation-related fund event, got: {:?}",
        check.last_fund_events().iter().map(|e| e.event_type).collect::<Vec<_>>()
    );
}

#[test]
fn hedge_funding_settlement_is_worker_invariant() {
    // The funding scan reads both the symbol leg AND the -symbol leg for HEDGE users
    // (user_funding_contribution's HEDGE branch). Every worker-invariance e2e above is
    // ONEWAY; this builds genuine dual-leg HEDGE users so that dual-leg path is covered.
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::margin_mode::MarginMode;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::exchange_api::PlaceFuturesOrderRequest;

    const BASE: i32 = 1;
    const QUOTE: i32 = 2;
    const FUT_SYMBOL: i32 = 906;
    const NUM_HEDGE: i64 = 100;
    const PX: i64 = 100;
    const SIZE: i64 = 10;
    const BAL: i64 = 10_000_000;
    const FIRST_HEDGE_UID: i64 = 1;

    fn place(api: &mut ExchangeApi, order_id: i64, uid: i64, action: OrderAction) {
        api.place_futures_order(PlaceFuturesOrderRequest {
            order_id, uid, symbol: FUT_SYMBOL, price: PX, size: SIZE, action,
            order_type: OrderType::Gtc, leverage: 1, margin_mode: MarginMode::Isolated,
            reduce_only: false,
        });
    }

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
        api.set_mark_price(FUT_SYMBOL, PX);

        let mut order_id: i64 = 1;
        for i in 0..NUM_HEDGE {
            let hedge_uid = i * 3 + 1;
            let cp_long = i * 3 + 2;
            let cp_short = i * 3 + 3;
            for uid in [hedge_uid, cp_long, cp_short] {
                api.add_user(uid);
                api.balance_adjustment(uid, QUOTE, BAL, uid);
            }
            api.adjust_position_mode(hedge_uid, true);

            // Long leg: hedge bids (rests), cp_long asks (fully matches -> no resting remainder).
            place(api, order_id, hedge_uid, OrderAction::Bid);
            order_id += 1;
            place(api, order_id, cp_long, OrderAction::Ask);
            order_id += 1;
            // Short leg (HEDGE second direction at -symbol): hedge asks (rests), cp_short bids.
            place(api, order_id, hedge_uid, OrderAction::Ask);
            order_id += 1;
            place(api, order_id, cp_short, OrderAction::Bid);
            order_id += 1;
        }
    }

    fn run(api: &mut ExchangeApi) {
        // action=Bid -> the LONG leg pays, the -symbol SHORT leg receives.
        api.settle_funding_fees(FUT_SYMBOL, OrderAction::Bid, 100, 1000, 999_999);
    }

    assert_workers_equivalent(build, run);

    // Non-vacuous: the first hedge user must actually hold both legs, and funding must
    // have debited its LONG (payer) leg, plus a settlement event must have fired.
    use exchange_core_rs::core::common::fund_event::FundEventType;
    let mut check = ExchangeApi::new();
    build(&mut check);
    let long_before = check.user_position(FIRST_HEDGE_UID, FUT_SYMBOL).expect("hedge long leg (key=symbol) must exist").profit;
    assert!(
        check.user_position(FIRST_HEDGE_UID, -FUT_SYMBOL).is_some(),
        "hedge user must hold a SHORT leg at -symbol"
    );
    run(&mut check);
    let long_after = check.user_position(FIRST_HEDGE_UID, FUT_SYMBOL).expect("hedge long leg must still exist").profit;
    assert!(long_after < long_before, "action=Bid funding must debit the hedge user's LONG (payer) leg profit");
    assert!(
        check.last_fund_events().iter().any(|e| e.event_type == FundEventType::FundingfeeSettlement),
        "HEDGE funding settlement must produce at least one FundingfeeSettlement event"
    );
}

// =====================================================================================
// Task 1: full-coverage random-stream property test + repeat-run / worker-matrix checks.
//
// A single rich world exercises all 4 parallelized O(N) batch scans:
//   - funding fee settlement          (SETTLE_FUNDINGFEES commands)
//   - futures liquidation scan        (MarkpriceAdjustment on RICH_FUT_SYMBOL)
//   - loan liquidation scan, isolated + cross (same MarkpriceAdjustment: RICH_FUT_SYMBOL
//     shares its base/quote currencies with RICH_SYMBOL's loan book, so decide_cross
//     runs too; the isolated cohort shares RICH_SYMBOL directly)
// =====================================================================================

const RICH_BASE: i32 = 1;
const RICH_QUOTE: i32 = 2;
const RICH_FUT_SYMBOL: i32 = 960;
const RICH_SYMBOL: i32 = 961;
const RICH_LP: i64 = 9_001;
const RICH_ENTRY: i64 = 10_000;
const RICH_NUM_USERS: usize = 8;
// 55% LTV at RICH_ENTRY (healthy, under the 60%/65% initial-borrow caps) but breaches
// the 80%/85% liquidation thresholds once price halves toward the bottom of the
// generator's 5_000..=15_000 range, so the loan scan gets genuine liquidation activity,
// not just a no-op pass over healthy loans.
const RICH_LOAN_PRINCIPAL: i64 = 550_000;

fn rich_world_uids() -> Vec<i64> {
    (1..=RICH_NUM_USERS as i64).collect()
}

fn build_rich_world(api: &mut ExchangeApi) -> Vec<i64> {
    use exchange_core_rs::core::common::core_currency_specification::CoreCurrencySpecification;
    use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
    use exchange_core_rs::core::common::isolated_loan_record::LoanRateMode;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::common::symbol_type::SymbolType;
    use exchange_core_rs::core::exchange_api::PlaceOrderRequest;
    use std::collections::BTreeMap;

    api.add_currencies([
        CoreCurrencySpecification { currency: RICH_BASE, currency_scale_k: 1, collateral_weight_bps: 10_000, ..Default::default() },
        CoreCurrencySpecification { currency: RICH_QUOTE, currency_scale_k: 1, ..Default::default() },
    ]);

    let mut mm = BTreeMap::new();
    mm.insert(1_000i64, 5i64);
    mm.insert(100_000i64, 10i64);
    api.add_futures_symbol(CoreSymbolSpecification {
        symbol_id: RICH_FUT_SYMBOL,
        symbol_type: SymbolType::FuturesContractPerpetual,
        base_currency: RICH_BASE,
        quote_currency: RICH_QUOTE,
        base_scale_k: 1,
        quote_scale_k: 1,
        maintenance_margin: mm,
        maintenance_margin_scale_k: 1_000,
        ..Default::default()
    });
    api.set_mark_price(RICH_FUT_SYMBOL, RICH_ENTRY);

    let mut loan_spec = CoreSymbolSpecification {
        symbol_id: RICH_SYMBOL,
        symbol_type: SymbolType::CurrencyExchangePair,
        base_currency: RICH_BASE,
        quote_currency: RICH_QUOTE,
        base_scale_k: 1,
        quote_scale_k: 1,
        ..Default::default()
    };
    loan_spec.loan_config.update(6_000, 8_000, 7_000, i64::MAX, 365);
    api.add_symbol(loan_spec);
    api.set_mark_price(RICH_SYMBOL, RICH_ENTRY);

    api.enable_liquidation();
    api.core().risk.loan_service.global_config.numeraire_currency = RICH_QUOTE;

    api.pool_deposit(RICH_QUOTE, 1_000_000_000, 1);

    let uids = rich_world_uids();
    let mut order_id: i64 = 100;
    for &uid in &uids {
        api.add_user(uid);
        api.balance_adjustment(uid, RICH_BASE, 1_000_000, order_id);
        order_id += 1;
        api.balance_adjustment(uid, RICH_QUOTE, 1_000_000, order_id);
        order_id += 1;
    }

    // Isolated loans: uids[0], uids[1].
    api.loan_create(order_id, uids[0], RICH_SYMBOL, 1, 100, RICH_LOAN_PRINCIPAL, LoanRateMode::Locked, 0);
    order_id += 1;
    api.loan_create(order_id, uids[1], RICH_SYMBOL, 1, 100, RICH_LOAN_PRINCIPAL, LoanRateMode::Locked, 0);
    order_id += 1;

    // Cross loans: uids[2], uids[3]. Collateral in RICH_BASE -- the same currency
    // RICH_FUT_SYMBOL is denominated in -- so a futures markprice adjustment also
    // re-triggers decide_cross for these two users.
    api.loan_cross_add_collateral(order_id, uids[2], RICH_BASE, 100, 0);
    order_id += 1;
    api.loan_cross_borrow(order_id, uids[2], RICH_SYMBOL, 1, RICH_LOAN_PRINCIPAL, 0);
    order_id += 1;
    api.loan_cross_add_collateral(order_id, uids[3], RICH_BASE, 100, 0);
    order_id += 1;
    api.loan_cross_borrow(order_id, uids[3], RICH_SYMBOL, 1, RICH_LOAN_PRINCIPAL, 0);
    order_id += 1;

    // LP resting bid so isolated/cross force-liquidation IOC sells have somewhere to fill.
    api.add_user(RICH_LP);
    api.balance_adjustment(RICH_LP, RICH_QUOTE, 1_000_000_000, order_id);
    order_id += 1;
    api.place_order(PlaceOrderRequest {
        order_id,
        uid: RICH_LP,
        symbol: RICH_SYMBOL,
        price: 1,
        size: 1_000_000,
        reserve_bid_price: 1,
        action: OrderAction::Bid,
        order_type: OrderType::Gtc,
    });

    uids
}

#[derive(Debug, Clone)]
enum ParaGenCmd {
    PlaceFutures { uid_idx: usize, is_bid: bool, price: i64, size: i64, leverage: i32 },
    MarkPrice { price: i64 },
    SettleFunding { is_ask: bool, rate: i64 },
}

fn para_cmd_strategy() -> impl Strategy<Value = ParaGenCmd> {
    let place = (0..RICH_NUM_USERS, any::<bool>(), 5_000i64..=15_000, 1i64..=20, 1i32..=20)
        .prop_map(|(uid_idx, is_bid, price, size, leverage)| ParaGenCmd::PlaceFutures { uid_idx, is_bid, price, size, leverage });
    let mark = (5_000i64..=15_000).prop_map(|price| ParaGenCmd::MarkPrice { price });
    let funding = (any::<bool>(), 1i64..=1_000).prop_map(|(is_ask, rate)| ParaGenCmd::SettleFunding { is_ask, rate });
    prop_oneof![4 => place, 3 => mark, 2 => funding]
}

fn apply_para_cmd(api: &mut ExchangeApi, cmd: &ParaGenCmd, uids: &[i64], order_id: i64) {
    use exchange_core_rs::core::common::margin_mode::MarginMode;
    use exchange_core_rs::core::common::order_action::OrderAction;
    use exchange_core_rs::core::common::order_type::OrderType;
    use exchange_core_rs::core::exchange_api::PlaceFuturesOrderRequest;

    match cmd {
        ParaGenCmd::PlaceFutures { uid_idx, is_bid, price, size, leverage } => {
            let uid = uids[*uid_idx % uids.len()];
            let _ = api.place_futures_order(PlaceFuturesOrderRequest {
                order_id,
                uid,
                symbol: RICH_FUT_SYMBOL,
                price: *price,
                size: *size,
                action: if *is_bid { OrderAction::Bid } else { OrderAction::Ask },
                // IOC, not GTC: a GTC order can leave a resting remainder that outlives
                // its owner (e.g. the owner's position on this symbol gets torn down by
                // a later liquidation/ADL pass); that stale resting order is still
                // matchable as a maker, and the engine's margin-settlement path panics
                // ("maker position record missing") when it is. That's a real,
                // reproducible bug independent of worker count (confirmed to reproduce
                // identically at workers=1) -- see careful-testing-report.md. It's a
                // pre-existing production defect unrelated to parallel batch compute and
                // out of scope for this test-only task, so IOC sidesteps it here rather
                // than letting it drown out the worker-invariance signal this test exists
                // to check.
                order_type: OrderType::Ioc,
                leverage: *leverage,
                margin_mode: MarginMode::Isolated,
                reduce_only: false,
            });
        }
        ParaGenCmd::MarkPrice { price } => {
            let _ = api.set_mark_price(RICH_FUT_SYMBOL, *price);
        }
        ParaGenCmd::SettleFunding { is_ask, rate } => {
            let action = if *is_ask { exchange_core_rs::core::common::order_action::OrderAction::Ask } else { exchange_core_rs::core::common::order_action::OrderAction::Bid };
            let _ = api.settle_funding_fees(RICH_FUT_SYMBOL, action, *rate, 1_000_000, order_id);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    #[test]
    fn full_coverage_random_stream_is_worker_invariant_after_each_command(
        cmds in prop::collection::vec(para_cmd_strategy(), 1..40)
    ) {
        let mut a = ExchangeApi::new();
        a.with_compute_pool(ComputeConfig { workers: 1, serial_threshold: 1024 });
        let uids_a = build_rich_world(&mut a);

        let mut b = ExchangeApi::new();
        b.with_compute_pool(ComputeConfig { workers: 8, serial_threshold: 0 });
        let uids_b = build_rich_world(&mut b);

        prop_assert_eq!(&uids_a, &uids_b);
        prop_assert_eq!(a.state_hash(), b.state_hash(), "engines diverged immediately after identical builds");

        let mut order_id: i64 = 1_000_000;
        for cmd in &cmds {
            apply_para_cmd(&mut a, cmd, &uids_a, order_id);
            apply_para_cmd(&mut b, cmd, &uids_b, order_id);
            order_id += 1;
            prop_assert_eq!(
                a.state_hash(),
                b.state_hash(),
                "workers=1 vs workers=8 state_hash diverged after cmd {:?}",
                cmd
            );
        }
    }
}

// =====================================================================================
// Task 1b: repeat-run determinism (same stream, same worker count, many runs) and a
// worker-count matrix (2 / 4 / 8) on one representative stream.
// =====================================================================================

fn fixed_rich_stream() -> Vec<ParaGenCmd> {
    vec![
        ParaGenCmd::PlaceFutures { uid_idx: 0, is_bid: true, price: 10_000, size: 5, leverage: 20 },
        ParaGenCmd::PlaceFutures { uid_idx: 1, is_bid: false, price: 10_000, size: 5, leverage: 20 },
        ParaGenCmd::PlaceFutures { uid_idx: 2, is_bid: true, price: 10_100, size: 3, leverage: 10 },
        ParaGenCmd::PlaceFutures { uid_idx: 3, is_bid: false, price: 10_100, size: 3, leverage: 10 },
        ParaGenCmd::MarkPrice { price: 9_500 },
        ParaGenCmd::SettleFunding { is_ask: false, rate: 50 },
        ParaGenCmd::PlaceFutures { uid_idx: 4, is_bid: true, price: 9_500, size: 8, leverage: 15 },
        ParaGenCmd::PlaceFutures { uid_idx: 5, is_bid: false, price: 9_500, size: 8, leverage: 15 },
        ParaGenCmd::MarkPrice { price: 8_800 },
        ParaGenCmd::PlaceFutures { uid_idx: 6, is_bid: true, price: 8_800, size: 4, leverage: 5 },
        ParaGenCmd::PlaceFutures { uid_idx: 7, is_bid: false, price: 8_800, size: 4, leverage: 5 },
        ParaGenCmd::SettleFunding { is_ask: true, rate: 30 },
        ParaGenCmd::MarkPrice { price: 11_500 },
        ParaGenCmd::PlaceFutures { uid_idx: 0, is_bid: false, price: 11_500, size: 2, leverage: 20 },
        ParaGenCmd::PlaceFutures { uid_idx: 1, is_bid: true, price: 11_500, size: 2, leverage: 20 },
        ParaGenCmd::MarkPrice { price: 7_500 },
        ParaGenCmd::SettleFunding { is_ask: false, rate: 200 },
        ParaGenCmd::MarkPrice { price: 10_200 },
        ParaGenCmd::PlaceFutures { uid_idx: 2, is_bid: false, price: 10_200, size: 6, leverage: 8 },
        ParaGenCmd::PlaceFutures { uid_idx: 3, is_bid: true, price: 10_200, size: 6, leverage: 8 },
        ParaGenCmd::MarkPrice { price: 6_500 },
        ParaGenCmd::SettleFunding { is_ask: true, rate: 500 },
        ParaGenCmd::MarkPrice { price: 12_800 },
        ParaGenCmd::PlaceFutures { uid_idx: 4, is_bid: false, price: 12_800, size: 10, leverage: 3 },
        ParaGenCmd::PlaceFutures { uid_idx: 5, is_bid: true, price: 12_800, size: 10, leverage: 3 },
        ParaGenCmd::MarkPrice { price: 9_900 },
    ]
}

fn run_fixed_stream(api: &mut ExchangeApi, uids: &[i64]) -> exchange_core_rs::core::reports::StateHashReport {
    let mut order_id: i64 = 2_000_000;
    for cmd in fixed_rich_stream().iter() {
        apply_para_cmd(api, cmd, uids, order_id);
        order_id += 1;
    }
    api.state_hash()
}

#[test]
fn repeat_runs_under_workers8_are_deterministic() {
    const N_RUNS: usize = 25;
    let mut hashes = Vec::with_capacity(N_RUNS);
    for _ in 0..N_RUNS {
        let mut api = ExchangeApi::new();
        api.with_compute_pool(ComputeConfig { workers: 8, serial_threshold: 0 });
        let uids = build_rich_world(&mut api);
        hashes.push(run_fixed_stream(&mut api, &uids));
    }
    for (i, h) in hashes.iter().enumerate().skip(1) {
        assert_eq!(&hashes[0], h, "run {i} of {N_RUNS} diverged from run 0 under workers=8 (scheduling-dependent nondeterminism)");
    }
}

#[test]
fn worker_count_matrix_is_state_hash_invariant() {
    let mut reference: Option<exchange_core_rs::core::reports::StateHashReport> = None;
    for &workers in &[2usize, 4, 8] {
        let mut api = ExchangeApi::new();
        api.with_compute_pool(ComputeConfig { workers, serial_threshold: 0 });
        let uids = build_rich_world(&mut api);
        let h = run_fixed_stream(&mut api, &uids);
        match &reference {
            None => reference = Some(h),
            Some(r) => assert_eq!(r, &h, "workers={workers} state_hash diverged from the workers=2 baseline"),
        }
    }
}
