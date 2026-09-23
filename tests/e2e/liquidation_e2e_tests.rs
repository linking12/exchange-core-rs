use proptest::prelude::*;

use exchange_core_rs::core::common::cmd::command_result_code::CommandResultCode;
use exchange_core_rs::core::common::cmd::order_command::OrderCommand;
use exchange_core_rs::core::common::cmd::order_command_type::OrderCommandType;
use exchange_core_rs::core::common::core_currency_specification::CoreCurrencySpecification;
use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
use exchange_core_rs::core::common::margin_mode::MarginMode;
use exchange_core_rs::core::common::order_action::OrderAction;
use exchange_core_rs::core::common::order_type::OrderType;
use exchange_core_rs::core::common::position_direction::PositionDirection;
use exchange_core_rs::core::common::symbol_type::SymbolType;

use exchange_core_rs::core::exchange_core::ExchangeCore;

const BASE: i32 = 1;
const QUOTE: i32 = 2;
const FUT: i32 = 500;

fn conserved(core: &ExchangeCore, cur: i32) -> i64 {
    let mark = core.risk.last_price_cache.get(&FUT).map(|r| r.mark_price).unwrap_or(0);
    let mut total: i64 = core.ups.users.values().map(|u| u.account(cur)).sum();
    total += *core.risk.fees.get(&cur).unwrap_or(&0);
    total += *core.risk.adjustments.get(&cur).unwrap_or(&0);
    for u in core.ups.users.values() {
        for p in u.positions.values() {
            if p.currency == cur {
                total += p.estimate_pnl(mark) + p.extra_margin;
            }
        }
    }
    if cur == QUOTE {
        for n in core.risk.liquidation_service.notionals.values() {
            total += n.available;
        }
        for ifp in core.risk.liquidation_service.positions.values() {
            total += ifp.position_value(mark);
        }
    }
    total
}

fn assert_if_non_negative(core: &ExchangeCore) {
    for n in core.risk.liquidation_service.notionals.values() {
        assert!(n.available >= 0, "IFNotional.available must not be negative: {}", n.available);
    }
}

fn fut_spec() -> CoreSymbolSpecification {
    let mut mm = std::collections::BTreeMap::new();
    mm.insert(i64::MAX, 500);
    CoreSymbolSpecification {
        symbol_id: FUT,
        symbol_type: SymbolType::FuturesContractPerpetual,
        base_currency: BASE,
        quote_currency: QUOTE,
        base_scale_k: 1,
        quote_scale_k: 1,
        taker_fee: 0,
        maker_fee: 0,
        fee_scale_k: 10_000,
        maintenance_margin: mm,
        maintenance_margin_scale_k: 10_000,
        liquidation_fee: 200,
        ..Default::default()
    }
}

fn seeded(n_users: i64) -> (ExchangeCore, Vec<i64>) {
    let mut core = ExchangeCore::new();
    core.ssp.add_currency(CoreCurrencySpecification { currency: BASE, currency_scale_k: 1, ..Default::default() });
    core.ssp.add_currency(CoreCurrencySpecification { currency: QUOTE, currency_scale_k: 1, ..Default::default() });
    assert_eq!(core.ssp.add_symbol(fut_spec()), CommandResultCode::Success);
    core.matching.add_symbol(&fut_spec());
    let uids: Vec<i64> = (1..=n_users).collect();
    for &uid in &uids {
        core.ups.add_empty_user_profile(uid);
        core.ups.get_mut(uid).unwrap().add_to_account(QUOTE, 1_000_000);
    }
    core.risk.liquidation_engine.is_running = true;
    (core, uids)
}

fn place(core: &mut ExchangeCore, order_id: i64, uid: i64, price: i64, size: i64, bid: bool, leverage: i32) {
    let mut c = OrderCommand {
        command: OrderCommandType::PlaceOrder,
        order_id,
        uid,
        symbol: FUT,
        price,
        size,
        reserve_bid_price: price,
        action: Some(if bid { OrderAction::Bid } else { OrderAction::Ask }),
        order_type: Some(OrderType::Gtc),
        leverage,
        margin_mode: MarginMode::Isolated,
        timestamp: 1_000,
        ..Default::default()
    };
    core.process_command(&mut c);
}

fn markprice(core: &mut ExchangeCore, price: i64, ts: i64) {
    let mut c = OrderCommand { command: OrderCommandType::MarkpriceAdjustment, symbol: FUT, price, timestamp: ts, ..Default::default() };
    core.process_command(&mut c);
}

/// Ask side volume resting at `price` in the live order book, via a non-mutating
/// `OrderBookRequest` L2 query (does not touch positions/margin at all).
fn resting_ask_volume_at(core: &mut ExchangeCore, price: i64) -> i64 {
    let mut c = OrderCommand { command: OrderCommandType::OrderBookRequest, symbol: FUT, size: 50, ..Default::default() };
    core.process_command(&mut c);
    let l2 = c.market_data.expect("OrderBookRequest should populate market_data");
    l2.ask_prices
        .iter()
        .zip(l2.ask_volumes.iter())
        .find(|(p, _)| **p == price)
        .map(|(_, v)| *v)
        .unwrap_or(0)
}

/// Bid side volume resting at `price` in the live order book (see `resting_ask_volume_at`).
fn resting_bid_volume_at(core: &mut ExchangeCore, price: i64) -> i64 {
    let mut c = OrderCommand { command: OrderCommandType::OrderBookRequest, symbol: FUT, size: 50, ..Default::default() };
    core.process_command(&mut c);
    let l2 = c.market_data.expect("OrderBookRequest should populate market_data");
    l2.bid_prices
        .iter()
        .zip(l2.bid_volumes.iter())
        .find(|(p, _)| **p == price)
        .map(|(_, v)| *v)
        .unwrap_or(0)
}

#[test]
fn force_full_fill_moves_fee_to_if_and_conserves() {
    let (mut core, uids) = seeded(3);
    let (m1, borrower, m2) = (uids[0], uids[1], uids[2]);
    markprice(&mut core, 100, 1_000);

    place(&mut core, 100, m1, 100, 10, false, 10);
    place(&mut core, 101, borrower, 100, 10, true, 10);
    assert_eq!(core.ups.get(borrower).unwrap().positions[&FUT].direction, PositionDirection::Long);
    place(&mut core, 102, m2, 92, 10, true, 10);

    let before_q = conserved(&core, QUOTE);
    let before_b = conserved(&core, BASE);

    markprice(&mut core, 94, 2_000);

    assert!(!core.ups.get(borrower).unwrap().positions.contains_key(&FUT), "borrower position should be fully closed");
    let if_avail: i64 = core.risk.liquidation_service.notionals.values().map(|n| n.available).sum();
    assert!(if_avail > 0, "liquidation fee should flow into IF");
    assert_eq!(conserved(&core, QUOTE), before_q, "QUOTE (including IF) should be conserved");
    assert_eq!(conserved(&core, BASE), before_b, "BASE should be conserved");
    assert_if_non_negative(&core);
}

/// Correct-behavior test for the fixed `maker position record missing` bug
/// (`src/core/processors/risk_engine.rs`, `settle_margin_position_event`). The
/// production fix guards the two `pending_release(...)` call sites in the Trade and
/// Reject/Reduce branches with `if !is_liquidation`, so a synthetic liquidation order
/// -- which never held any `pending_*` reservation of its own -- no longer drains the
/// `pending_sell_size`/`pending_buy_size` that actually belongs to an unrelated,
/// still-resting order sharing the same (per-side, per-symbol) position key.
///
/// Sequence (all on one symbol, OneWay position mode so there's a single shared
/// position record per user per symbol):
///  1. `m1` rests an ASK at 100; `x` buys into it -> `x` is fully-filled Long 10 @100
///     (no leftover pending on x's position).
///  2. `x` places a SECOND, unrelated GTC ASK (size 3 @110) that does not cross
///     anything and simply rests on the book. This sets `x`'s position record's
///     `pending_sell_size = 3` (aggregated per side+symbol, not per order).
///  3. `m2` rests a BID at 92 to provide exit liquidity for the coming liquidation.
///  4. Mark price crashes to 94. `x`'s highly-leveraged long is force-liquidated: the
///     engine submits a synthetic IOC SELL of size 10 that matches `m2`'s resting bid
///     in one trade. `x` is the TAKER of that trade. With the fix, because this is a
///     liquidation, `pending_release` is skipped entirely -- x's real resting ask from
///     step 2 keeps its `pending_sell_size = 3` untouched, so x's position record is
///     RETAINED (not empty) even though `open_volume` closed to 0. x's resting ask
///     order also remains live in the order book the whole time.
///  5. `m3` buys into x's still-resting ask at 110. `x` is the MAKER of this trade;
///     the engine finds x's (retained) position record for the maker leg and settles
///     normally -- no panic -- opening a fresh Short 3 @110 position for x.
#[test]
fn stale_resting_order_survives_liquidation() {
    let (mut core, uids) = seeded(4);
    let (m1, x, m2, m3) = (uids[0], uids[1], uids[2], uids[3]);

    markprice(&mut core, 100, 1_000);

    // 1. x opens a fully-filled long against m1.
    place(&mut core, 100, m1, 100, 10, false, 10);
    place(&mut core, 101, x, 100, 10, true, 10);
    assert_eq!(core.ups.get(x).unwrap().positions[&FUT].direction, PositionDirection::Long);
    assert_eq!(core.ups.get(x).unwrap().positions[&FUT].open_volume, 10);

    // 2. x's second, unrelated resting order on the SAME symbol -- does not cross,
    //    just rests, holding pending_sell_size = 3 on x's (shared, OneWay) position key.
    place(&mut core, 102, x, 110, 3, false, 10);
    assert_eq!(core.ups.get(x).unwrap().positions[&FUT].pending_sell_size, 3);
    assert_eq!(resting_ask_volume_at(&mut core, 110), 3, "x's resting ask should be live in the book");

    // 3. Exit liquidity for the coming liquidation.
    place(&mut core, 103, m2, 92, 10, true, 10);

    let before_q = conserved(&core, QUOTE);
    let before_b = conserved(&core, BASE);

    // 4. Crash the mark price -- liquidates x's long via a synthetic IOC sell matched
    //    against m2's resting bid. With the fix, x's unrelated pending_sell_size (3)
    //    is left alone, so x's position record survives (retained, not empty).
    markprice(&mut core, 94, 2_000);
    assert!(core.ups.get(x).unwrap().positions.contains_key(&FUT), "x's position record must be RETAINED after liquidation (fix)");
    let after_liq = &core.ups.get(x).unwrap().positions[&FUT];
    assert_eq!(after_liq.open_volume, 0, "x's liquidated long should be fully closed");
    assert_eq!(after_liq.pending_sell_size, 3, "x's unrelated resting-order pending must survive the liquidation");
    assert_eq!(resting_ask_volume_at(&mut core, 110), 3, "x's resting ask must still be live in the book after liquidation");
    assert_eq!(conserved(&core, QUOTE), before_q, "QUOTE conservation must hold across liquidation");
    assert_eq!(conserved(&core, BASE), before_b, "BASE conservation must hold across liquidation");

    // 5. m3 matches x's still-resting ask from step 2 as taker; x is the maker and the
    //    engine must find x's (retained) position record -- no panic -- and settle it
    //    into a fresh Short position.
    place(&mut core, 104, m3, 110, 3, true, 10);

    assert!(core.ups.get(x).unwrap().positions.contains_key(&FUT), "x's position should exist after the maker fill settles");
    let settled = &core.ups.get(x).unwrap().positions[&FUT];
    assert_eq!(settled.direction, PositionDirection::Short, "x's maker fill should open a fresh Short position");
    assert_eq!(settled.open_volume, 3, "x's fresh Short position should be sized to the maker fill");
    assert_eq!(settled.pending_sell_size, 0, "the maker fill should have released x's pending_sell_size");
    assert_eq!(resting_ask_volume_at(&mut core, 110), 0, "x's resting ask should be fully filled and gone from the book");
    assert_eq!(conserved(&core, QUOTE), before_q, "QUOTE conservation must hold across the later maker fill");
    assert_eq!(conserved(&core, BASE), before_b, "BASE conservation must hold across the later maker fill");
}

/// Test E (mirror, NEGATIVE): x is LONG and rests an UNRELATED order on the SAME side as
/// opening (a BID -- the add-to-position side), then gets force-liquidated. The liquidation
/// closes a LONG via a synthetic ASK, whose taker-side `pending_release(Ask, ...)` only ever
/// touches `pending_sell_size` -- never `pending_buy_size`. x's resting BID lives on
/// `pending_buy_size`, a different field on the same shared position record, so it is never
/// touched by the liquidation's release call regardless of the fix. `is_empty()` therefore
/// correctly stays false (pending_buy_size nonzero) whether or not the `!is_liquidation` guard
/// is present -- this scenario was never at risk. It documents the trigger boundary: the bug
/// needs the resting order on the side OPPOSITE the position (the side the liquidation's close
/// releases), not the same side.
#[test]
fn same_side_resting_order_never_at_risk_from_liquidation() {
    let (mut core, uids) = seeded(4);
    let (m1, x, m2, m3) = (uids[0], uids[1], uids[2], uids[3]);

    markprice(&mut core, 100, 1_000);

    // 1. x opens a fully-filled long against m1.
    place(&mut core, 100, m1, 100, 10, false, 10);
    place(&mut core, 101, x, 100, 10, true, 10);
    assert_eq!(core.ups.get(x).unwrap().positions[&FUT].direction, PositionDirection::Long);
    assert_eq!(core.ups.get(x).unwrap().positions[&FUT].open_volume, 10);

    // 2. x's unrelated SAME-side (BID) resting order -- well below any price that will trade,
    //    so it never crosses; registers pending_buy_size = 3 on the shared position record.
    place(&mut core, 102, x, 50, 3, true, 10);
    {
        let pos = &core.ups.get(x).unwrap().positions[&FUT];
        assert_eq!(pos.open_volume, 10, "open long size unchanged by the resting bid");
        assert_eq!(pos.pending_buy_size, 3, "resting bid registered as pending_buy_size on the SAME record");
        assert_eq!(pos.pending_sell_size, 0, "no pending_sell_size");
    }

    // 3. Exit liquidity for the coming liquidation (sized to fully consume, no leftover, so it
    //    doesn't sit at a better price than x's low same-side resting bid).
    place(&mut core, 103, m2, 92, 10, true, 10);

    let before_q = conserved(&core, QUOTE);
    let before_b = conserved(&core, BASE);

    // 4. Crash the mark price -- liquidates x's long via a synthetic IOC sell. This only ever
    //    releases pending_sell_size (0 here), so x's pending_buy_size (3) is untouched -- was
    //    never at risk, with or without the fix.
    markprice(&mut core, 94, 2_000);
    assert!(core.ups.get(x).unwrap().positions.contains_key(&FUT), "x's position record must be RETAINED (never at risk here)");
    let after_liq = &core.ups.get(x).unwrap().positions[&FUT];
    assert_eq!(after_liq.open_volume, 0, "x's liquidated long should be fully closed");
    assert_eq!(after_liq.pending_buy_size, 3, "x's same-side resting bid pending must be UNCHANGED by the liquidation's ASK-side release");
    assert_eq!(resting_bid_volume_at(&mut core, 50), 3, "x's resting bid must still be live in the book after liquidation");
    assert_eq!(conserved(&core, QUOTE), before_q, "QUOTE conservation must hold across liquidation");
    assert_eq!(conserved(&core, BASE), before_b, "BASE conservation must hold across liquidation");

    // 5. m3 crosses x's still-resting bid as taker; x is the maker and legitimately opens a
    //    fresh Long position -- no panic, whether or not the fix is present.
    place(&mut core, 104, m3, 50, 3, false, 10);

    assert!(core.ups.get(x).unwrap().positions.contains_key(&FUT), "x's position should exist after the maker fill settles");
    let settled = &core.ups.get(x).unwrap().positions[&FUT];
    assert_eq!(settled.direction, PositionDirection::Long, "x's maker fill should open a fresh Long position");
    assert_eq!(settled.open_volume, 3, "x's fresh Long position should be sized to the maker fill");
    assert_eq!(settled.pending_buy_size, 0, "the maker fill should have released x's pending_buy_size");
    assert_eq!(resting_bid_volume_at(&mut core, 50), 0, "x's resting bid should be fully filled and gone from the book");
    assert_eq!(conserved(&core, QUOTE), before_q, "QUOTE conservation must hold across the later maker fill");
    assert_eq!(conserved(&core, BASE), before_b, "BASE conservation must hold across the later maker fill");
}

/// Test F (mirror, POSITIVE: fix covers the BID/pending_buy_size direction too): mirror of the
/// main test with x SHORT instead of LONG, and an unrelated resting BID (the reduce/opposite
/// side of a SHORT, sharing `pending_buy_size`) instead of a resting ASK. A SHORT's
/// force-liquidation closes via a synthetic BID (buy-back), whose taker-side
/// `pending_release(Bid, ...)` pre-fix unconditionally drained `pending_buy_size` -- the SAME
/// field backing x's resting bid -- reproducing the identical bug via the buy-side instead of
/// the sell-side. Proves the `!is_liquidation` guard is direction-symmetric.
#[test]
fn short_side_resting_order_survives_liquidation() {
    let (mut core, uids) = seeded(4);
    let (m1, x, m2, m3) = (uids[0], uids[1], uids[2], uids[3]);

    markprice(&mut core, 100, 1_000);

    // 1. x opens a fully-filled SHORT against m1's resting bid.
    place(&mut core, 100, m1, 100, 10, true, 10);
    place(&mut core, 101, x, 100, 10, false, 10);
    assert_eq!(core.ups.get(x).unwrap().positions[&FUT].direction, PositionDirection::Short);
    assert_eq!(core.ups.get(x).unwrap().positions[&FUT].open_volume, 10);

    // 2. x's second, unrelated resting BID (opposite side of the SHORT) -- does not cross,
    //    just rests, holding pending_buy_size = 3 on x's shared position key.
    place(&mut core, 102, x, 50, 3, true, 10);
    assert_eq!(core.ups.get(x).unwrap().positions[&FUT].pending_buy_size, 3);
    assert_eq!(resting_bid_volume_at(&mut core, 50), 3, "x's resting bid should be live in the book");

    // 3. Exit liquidity (ASK) for the coming SHORT liquidation's buy-back.
    place(&mut core, 103, m2, 106, 10, false, 10);

    let before_q = conserved(&core, QUOTE);
    let before_b = conserved(&core, BASE);

    // 4. Push the mark price up -- liquidates x's short via a synthetic IOC buy matched
    //    against m2's resting ask. With the fix, x's unrelated pending_buy_size (3) is left
    //    alone, so x's position record survives (retained, not empty).
    markprice(&mut core, 106, 2_000);
    assert!(core.ups.get(x).unwrap().positions.contains_key(&FUT), "x's position record must be RETAINED after liquidation (fix)");
    let after_liq = &core.ups.get(x).unwrap().positions[&FUT];
    assert_eq!(after_liq.open_volume, 0, "x's liquidated short should be fully closed");
    assert_eq!(after_liq.pending_buy_size, 3, "x's unrelated resting-order pending must survive the liquidation");
    assert_eq!(resting_bid_volume_at(&mut core, 50), 3, "x's resting bid must still be live in the book after liquidation");
    assert_eq!(conserved(&core, QUOTE), before_q, "QUOTE conservation must hold across liquidation");
    assert_eq!(conserved(&core, BASE), before_b, "BASE conservation must hold across liquidation");

    // 5. m3 matches x's still-resting bid as taker (sells into it); x is the maker and the
    //    engine must find x's (retained) position record -- no panic -- and settle it into a
    //    fresh Long position.
    place(&mut core, 104, m3, 50, 3, false, 10);

    assert!(core.ups.get(x).unwrap().positions.contains_key(&FUT), "x's position should exist after the maker fill settles");
    let settled = &core.ups.get(x).unwrap().positions[&FUT];
    assert_eq!(settled.direction, PositionDirection::Long, "x's maker fill should open a fresh Long position");
    assert_eq!(settled.open_volume, 3, "x's fresh Long position should be sized to the maker fill");
    assert_eq!(settled.pending_buy_size, 0, "the maker fill should have released x's pending_buy_size");
    assert_eq!(resting_bid_volume_at(&mut core, 50), 0, "x's resting bid should be fully filled and gone from the book");
    assert_eq!(conserved(&core, QUOTE), before_q, "QUOTE conservation must hold across the later maker fill");
    assert_eq!(conserved(&core, BASE), before_b, "BASE conservation must hold across the later maker fill");
}

#[test]
fn healthy_market_no_liquidation_conserves() {
    let (mut core, uids) = seeded(2);
    markprice(&mut core, 100, 1_000);
    place(&mut core, 100, uids[0], 100, 10, false, 5);
    place(&mut core, 101, uids[1], 100, 10, true, 5);
    let before = conserved(&core, QUOTE);
    markprice(&mut core, 101, 2_000);
    assert!(core.ups.get(uids[1]).unwrap().positions.contains_key(&FUT), "healthy position should not be liquidated");
    assert_eq!(conserved(&core, QUOTE), before);
}

#[derive(Debug, Clone)]
enum GenCmd {
    Place { uid_idx: usize, price: i64, size: i64 },
    Mark { price: i64 },
}

fn cmd_strategy() -> impl Strategy<Value = GenCmd> {
    prop_oneof![
        (0usize..4, 80i64..120, 1i64..20).prop_map(|(uid_idx, price, size)| GenCmd::Place { uid_idx, price, size }),
        (60i64..140).prop_map(|price| GenCmd::Mark { price }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 120, ..ProptestConfig::default() })]

    #[test]
    fn conservation_holds_under_random_stream_with_liquidation(cmds in prop::collection::vec(cmd_strategy(), 1..40)) {
        let (mut core, uids) = seeded(4);
        markprice(&mut core, 100, 1_000);

        let base_q = conserved(&core, QUOTE);
        let base_b = conserved(&core, BASE);

        let mut oid: i64 = 1000;
        let mut ts: i64 = 2_000;
        for cmd in &cmds {
            match cmd {
                GenCmd::Place { uid_idx, price, size } => {
                    let bid = uid_idx % 2 == 0;
                    place(&mut core, oid, uids[*uid_idx], *price, *size, bid, 10);
                    oid += 1;
                }
                GenCmd::Mark { price } => {
                    markprice(&mut core, *price, ts);
                    ts += 1_000;
                }
            }
            prop_assert_eq!(conserved(&core, QUOTE), base_q, "QUOTE conservation violated");
            prop_assert_eq!(conserved(&core, BASE), base_b, "BASE conservation violated");
            for n in core.risk.liquidation_service.notionals.values() {
                prop_assert!(n.available >= 0, "IFNotional.available is negative");
            }
        }
    }
}
