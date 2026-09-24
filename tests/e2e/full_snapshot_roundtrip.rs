use exchange_core_rs::core::common::cmd::command_result_code::CommandResultCode;
use exchange_core_rs::core::common::cmd::order_command::OrderCommand;
use exchange_core_rs::core::common::cmd::order_command_type::OrderCommandType;
use exchange_core_rs::core::common::core_currency_specification::CoreCurrencySpecification;
use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
use exchange_core_rs::core::common::isolated_loan_record::LoanRateMode;
use exchange_core_rs::core::common::last_price_cache_record::LastPriceCacheRecord;
use exchange_core_rs::core::common::margin_mode::MarginMode;
use exchange_core_rs::core::common::order_action::OrderAction;
use exchange_core_rs::core::common::order_type::OrderType;
use exchange_core_rs::core::common::position_direction::PositionDirection;
use exchange_core_rs::core::common::symbol_type::SymbolType;
use exchange_core_rs::core::exchange_api::{ExchangeApi, MarginAdjustmentRequest, PlaceFuturesOrderRequest, PlaceOrderRequest};
use exchange_core_rs::core::exchange_core::ExchangeCore;
use exchange_core_rs::core::snapshot::serialization_processor::{
    InMemorySerializationProcessor, SerializationProcessor, SerializedModuleType,
};

const BTC: i32 = 1;
const USDT: i32 = 2;
const LWBTC: i32 = 720;
const LUSDT: i32 = 721;

const SPOT: i32 = 100;
const FUT: i32 = 200;
const LOAN: i32 = 72010;

const FUT_MARK: i64 = 100;
const LOAN_MARK: i64 = 50_000;
const LOAN_POOL: i64 = 10_000_000;

const SPOT_MAKER: i64 = 1;
const SPOT_TAKER: i64 = 2;
const ISO_LONG: i64 = 10;
const ISO_SHORT: i64 = 11;
const CROSS_LONG: i64 = 20;
const CROSS_SHORT: i64 = 21;
const HEDGE: i64 = 30;
const HEDGE_CP_A: i64 = 31;
const HEDGE_CP_B: i64 = 32;
const LOAN_USER: i64 = 40;
const LP: i64 = 50;
const SUSPENDED: i64 = 60;

const ISO_LOAN_ID: i64 = 1;
const CROSS_LOAN_ID: i64 = 2;

fn raw_submit(core: &mut ExchangeCore, mut cmd: OrderCommand) -> CommandResultCode {
    core.process_command(&mut cmd);
    cmd.result_code.expect("every command produces a result code")
}

fn spot_spec(symbol_id: i32, base: i32, quote: i32) -> CoreSymbolSpecification {
    CoreSymbolSpecification {
        symbol_id,
        symbol_type: SymbolType::CurrencyExchangePair,
        base_currency: base,
        quote_currency: quote,
        base_scale_k: 1,
        quote_scale_k: 1,
        taker_fee: 0,
        maker_fee: 0,
        fee_scale_k: 0,
        ..Default::default()
    }
}

fn futures_spec(symbol_id: i32, base: i32, quote: i32) -> CoreSymbolSpecification {
    CoreSymbolSpecification {
        symbol_id,
        symbol_type: SymbolType::FuturesContractPerpetual,
        base_currency: base,
        quote_currency: quote,
        base_scale_k: 1,
        quote_scale_k: 1,
        taker_fee: 0,
        maker_fee: 0,
        fee_scale_k: 0,
        ..Default::default()
    }
}

fn loan_spot_spec(symbol_id: i32, base: i32, quote: i32) -> CoreSymbolSpecification {
    let mut spec = spot_spec(symbol_id, base, quote);
    spec.loan_config.update(6_000, 8_500, 7_500, i64::MAX, 365);
    spec
}

fn futures_leg(api: &ExchangeApi, uid: i64, symbol: i32, dir: PositionDirection) -> Option<&exchange_core_rs::core::common::symbol_position_record::SymbolPositionRecord> {
    api.ups().get(uid)?.positions.values().find(|p| p.symbol == symbol && p.direction == dir)
}

fn place_futures(api: &mut ExchangeApi, order_id: i64, uid: i64, price: i64, size: i64, action: OrderAction, margin_mode: MarginMode) -> CommandResultCode {
    api.place_futures_order(PlaceFuturesOrderRequest {
        order_id,
        uid,
        symbol: FUT,
        price,
        size,
        action,
        order_type: OrderType::Gtc,
        leverage: 1,
        margin_mode,
        reduce_only: false,
    })
}

fn build_full_state(shared: &InMemorySerializationProcessor) -> ExchangeApi {
    let mut api = ExchangeApi::new();
    api.core().with_serialization_processor(Box::new(shared.clone()));

    api.add_currency(BTC, 1);
    api.add_currency(USDT, 1);
    api.core().ssp.add_currency(CoreCurrencySpecification { currency: LWBTC, currency_scale_k: 100, collateral_weight_bps: 10_000, ..Default::default() });
    api.core().ssp.add_currency(CoreCurrencySpecification { currency: LUSDT, currency_scale_k: 1, ..Default::default() });

    assert_eq!(api.add_symbol(spot_spec(SPOT, BTC, USDT)), CommandResultCode::Success);
    assert_eq!(api.add_futures_symbol(futures_spec(FUT, BTC, USDT)), CommandResultCode::Success);
    assert_eq!(api.add_symbol(loan_spot_spec(LOAN, LWBTC, LUSDT)), CommandResultCode::Success);

    assert_eq!(api.set_mark_price(FUT, FUT_MARK), CommandResultCode::Success);
    api.core().risk.last_price_cache.insert(LOAN, LastPriceCacheRecord::with_mark(LOAN_MARK));
    api.core().risk.loan_service.global_config.numeraire_currency = LUSDT;

    for uid in [SPOT_MAKER, SPOT_TAKER, ISO_LONG, ISO_SHORT, CROSS_LONG, CROSS_SHORT, HEDGE, HEDGE_CP_A, HEDGE_CP_B, LOAN_USER, LP] {
        assert_eq!(api.add_user(uid), CommandResultCode::Success);
    }

    assert_eq!(api.balance_adjustment(SPOT_MAKER, BTC, 1_000, 1), CommandResultCode::Success);
    assert_eq!(api.balance_adjustment(SPOT_TAKER, USDT, 100_000, 2), CommandResultCode::Success);
    for (i, uid) in [ISO_LONG, ISO_SHORT, CROSS_LONG, CROSS_SHORT, HEDGE, HEDGE_CP_A, HEDGE_CP_B].into_iter().enumerate() {
        assert_eq!(api.balance_adjustment(uid, USDT, 1_000_000, 100 + i as i64), CommandResultCode::Success);
    }
    assert_eq!(api.balance_adjustment(LOAN_USER, LWBTC, 1_000, 200), CommandResultCode::Success);
    assert_eq!(api.balance_adjustment(LP, LUSDT, LOAN_POOL, 201), CommandResultCode::Success);
    assert_eq!(api.balance_adjustment(LP, USDT, 50_000, 202), CommandResultCode::Success);

    assert_eq!(
        api.place_order(PlaceOrderRequest { order_id: 1_000, uid: SPOT_MAKER, symbol: SPOT, price: 50, size: 1_000, reserve_bid_price: 0, action: OrderAction::Ask, order_type: OrderType::Gtc }),
        CommandResultCode::Success
    );
    assert_eq!(
        api.place_order(PlaceOrderRequest { order_id: 1_001, uid: SPOT_TAKER, symbol: SPOT, price: 50, size: 400, reserve_bid_price: 50, action: OrderAction::Bid, order_type: OrderType::Gtc }),
        CommandResultCode::Success
    );
    assert_eq!(api.user_locked(SPOT_MAKER, BTC), 600, "spot maker keeps 600 base locked behind the resting ask");

    assert_eq!(place_futures(&mut api, 2_000, ISO_SHORT, FUT_MARK, 6, OrderAction::Ask, MarginMode::Isolated), CommandResultCode::Success);
    assert_eq!(place_futures(&mut api, 2_001, ISO_LONG, FUT_MARK, 6, OrderAction::Bid, MarginMode::Isolated), CommandResultCode::Success);
    assert_eq!(
        api.margin_adjustment(MarginAdjustmentRequest { uid: ISO_LONG, symbol: FUT, action: OrderAction::Bid, amount: 500, margin_mode: MarginMode::Isolated, order_id: 2_010 }),
        CommandResultCode::Success
    );
    assert_eq!(futures_leg(&api, ISO_LONG, FUT, PositionDirection::Long).unwrap().extra_margin, 500);

    assert_eq!(place_futures(&mut api, 3_000, CROSS_SHORT, FUT_MARK, 4, OrderAction::Ask, MarginMode::Cross), CommandResultCode::Success);
    assert_eq!(place_futures(&mut api, 3_001, CROSS_LONG, FUT_MARK, 4, OrderAction::Bid, MarginMode::Cross), CommandResultCode::Success);

    assert_eq!(api.adjust_position_mode(HEDGE, true), CommandResultCode::Success);
    assert_eq!(place_futures(&mut api, 4_000, HEDGE_CP_A, FUT_MARK, 5, OrderAction::Ask, MarginMode::Isolated), CommandResultCode::Success);
    assert_eq!(place_futures(&mut api, 4_001, HEDGE, FUT_MARK, 5, OrderAction::Bid, MarginMode::Isolated), CommandResultCode::Success);
    assert_eq!(place_futures(&mut api, 4_002, HEDGE_CP_B, FUT_MARK, 3, OrderAction::Bid, MarginMode::Isolated), CommandResultCode::Success);
    assert_eq!(place_futures(&mut api, 4_003, HEDGE, FUT_MARK, 3, OrderAction::Ask, MarginMode::Isolated), CommandResultCode::Success);
    assert_eq!(futures_leg(&api, HEDGE, FUT, PositionDirection::Long).unwrap().open_volume, 5);
    assert_eq!(futures_leg(&api, HEDGE, FUT, PositionDirection::Short).unwrap().open_volume, 3);

    assert_eq!(api.settle_funding_fees(FUT, OrderAction::Bid, 10, 10_000, 5_000), CommandResultCode::Success);

    assert_eq!(api.pool_deposit(LUSDT, LOAN_POOL, 6_000), CommandResultCode::Success);
    assert_eq!(api.loan_if_deposit(LUSDT, 5_000, 6_001), CommandResultCode::Success);
    assert_eq!(api.insurance_fund_deposit(FUT, 10_000, 6_002), CommandResultCode::Success);

    assert_eq!(api.loan_create(6_100, LOAN_USER, LOAN, ISO_LOAN_ID, 300, 80_000, LoanRateMode::Locked, 1_000), CommandResultCode::Success);
    assert_eq!(api.loan_cross_add_collateral(6_101, LOAN_USER, LWBTC, 300, 1_000), CommandResultCode::Success);
    assert_eq!(api.loan_cross_borrow(6_102, LOAN_USER, LOAN, CROSS_LOAN_ID, 60_000, 1_000), CommandResultCode::Success);

    assert_eq!(
        raw_submit(api.core(), OrderCommand { command: OrderCommandType::BalanceAdjustment, uid: LP, symbol: USDT, price: 1_000, order_id: 6_200, order_type: Some(OrderType::Ioc), ..Default::default() }),
        CommandResultCode::Success
    );
    assert_ne!(*api.core().risk.suspends.get(&USDT).unwrap_or(&0), 0, "suspend-typed balance adjustment populates the suspends bucket");

    assert_eq!(api.internal_transfer(LP, SUSPENDED, USDT, 7_000, 6_300), CommandResultCode::Success);
    assert_eq!(
        api.ups().get(SUSPENDED).map(|p| p.user_status),
        Some(exchange_core_rs::core::common::user_status::UserStatus::Suspended),
        "transfer to a never-seen uid auto-creates a persisted suspended user"
    );

    api
}

#[test]
fn all_subsystem_state_survives_snapshot_roundtrip_byte_identical() {
    let shared = InMemorySerializationProcessor::new();
    let mut leader = build_full_state(&shared);

    assert!(leader.core().persist(1, 0));

    let mut follower = ExchangeCore::new();
    follower.with_serialization_processor(Box::new(shared.clone()));
    follower.recover(1, 0);
    assert!(follower.persist(2, 0));

    for module in [SerializedModuleType::RiskEngine, SerializedModuleType::MatchingEngineRouter, SerializedModuleType::ExchangeCore] {
        assert_eq!(
            shared.load_data(2, module, 0),
            shared.load_data(1, module, 0),
            "recovered {module:?} module must be byte-identical to the leader"
        );
    }

    assert_eq!(follower.query_state_hash(), leader.core().query_state_hash(), "recovered replicated state hash must match the leader");

    assert_eq!(follower.ups.get(SPOT_MAKER).unwrap().locked(BTC), 600, "spot resting ask (exchange_locked) survives");
    assert_eq!(follower.ups.get(SPOT_TAKER).unwrap().account(USDT), leader.user_account(SPOT_TAKER, USDT), "spot partial-fill balances survive");

    let iso_long = follower.ups.get(ISO_LONG).unwrap().positions.values().find(|p| p.symbol == FUT && p.direction == PositionDirection::Long).unwrap();
    assert_eq!(iso_long.open_volume, 6);
    assert_eq!(iso_long.margin_mode, MarginMode::Isolated);
    assert_eq!(iso_long.extra_margin, 500, "isolated extra_margin from MARGIN_ADJUST survives");

    let cross_long = follower.ups.get(CROSS_LONG).unwrap().positions.values().find(|p| p.symbol == FUT && p.direction == PositionDirection::Long).unwrap();
    assert_eq!(cross_long.open_volume, 4);
    assert_eq!(cross_long.margin_mode, MarginMode::Cross);

    let hedge_long = follower.ups.get(HEDGE).unwrap().positions.values().find(|p| p.symbol == FUT && p.direction == PositionDirection::Long).unwrap();
    let hedge_short = follower.ups.get(HEDGE).unwrap().positions.values().find(|p| p.symbol == FUT && p.direction == PositionDirection::Short).unwrap();
    assert_eq!(hedge_long.open_volume, 5, "hedge long leg survives");
    assert_eq!(hedge_short.open_volume, 3, "hedge short leg survives");

    assert_eq!(follower.ups.get(LOAN_USER).unwrap().isolated_loans.get(&ISO_LOAN_ID).map(|l| l.outstanding_principal), Some(80_000), "isolated loan principal survives");
    assert_eq!(follower.ups.get(LOAN_USER).unwrap().cross_loans.get(&CROSS_LOAN_ID).map(|l| l.outstanding_principal), Some(60_000), "cross loan principal survives");

    assert_eq!(follower.risk.loan_service.loan_pool_available.get(&LUSDT), leader.core().risk.loan_service.loan_pool_available.get(&LUSDT), "loan pool balance survives");
    assert_eq!(follower.risk.loan_service.get_loan_insurance_fund(LUSDT), leader.core().risk.loan_service.get_loan_insurance_fund(LUSDT), "loan insurance fund survives");
    assert_eq!(follower.risk.suspends, leader.core().risk.suspends, "suspends bucket survives");
    assert_eq!(follower.risk.adjustments, leader.core().risk.adjustments, "adjustments bucket survives");
    assert_eq!(follower.risk.fees, leader.core().risk.fees, "fees bucket survives");
    assert_eq!(follower.ups.get(SUSPENDED).map(|p| p.user_status), Some(exchange_core_rs::core::common::user_status::UserStatus::Suspended), "suspended user survives");

    assert_eq!(follower.query_total_balance(), leader.core().query_total_balance(), "full balance report matches after recovery");

    let spot_fill = raw_submit(&mut follower, OrderCommand {
        command: OrderCommandType::PlaceOrder,
        order_id: 9_000,
        uid: SPOT_TAKER,
        symbol: SPOT,
        price: 50,
        size: 600,
        reserve_bid_price: 50,
        action: Some(OrderAction::Bid),
        order_type: Some(OrderType::Gtc),
        ..Default::default()
    });
    assert_eq!(spot_fill, CommandResultCode::Success, "rebuilt spot_pair_index lets the recovered book match the remaining resting ask");
    assert_eq!(follower.ups.get(SPOT_MAKER).unwrap().locked(BTC), 0, "remaining resting ask fully consumed after recovery");

    follower.risk.liquidation_engine.is_running = true;
    let scan = raw_submit(&mut follower, OrderCommand {
        command: OrderCommandType::LiquidationScan,
        order_id: 9_100,
        price: 0,
        size: 16,
        timestamp: 2_000,
        ..Default::default()
    });
    assert_eq!(scan, CommandResultCode::Success, "rebuilt liquidation indices make a targeted LiquidationScan usable after recovery");
}
