use exchange_core_rs::core::common::margin_mode::MarginMode;
use exchange_core_rs::core::common::position_direction::PositionDirection;
use exchange_core_rs::core::exchange_core::ExchangeCore;
use exchange_core_rs::core::processors::matching_engine_router::MatchingEngineRouter;
use exchange_core_rs::core::processors::risk_engine::read_risk_engine_payload;
use exchange_core_rs::core::snapshot::chronicle_reader::ChronicleReader;
use exchange_core_rs::core::snapshot::marshalling::ChronicleMarshallable;

const FIXTURE_DIR: &str = "/tmp/rust_snapshot_dat_fixture";

const USD: i32 = 1;
const USDT: i32 = 2;
const BTC: i32 = 3;
const ETH: i32 = 4;

const SPOT_BTC_USDT: i32 = 100;
const SPOT_ETH_USDT: i32 = 101;
const PERP_BTC: i32 = 200;

const U10: i64 = 10;
const U11: i64 = 11;
const U12: i64 = 12;
const U13: i64 = 13;
const U20: i64 = 20;
const U22: i64 = 22;

const ISO_LOAN_ID: i64 = 9001;
const CROSS_LOAN_ID: i64 = 9002;

fn strip_dat_framing(raw: &[u8]) -> Vec<u8> {
    assert!(raw.len() >= 8, "dat file too small to contain framing header");
    let outer_len = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let inner_len = i32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    assert_eq!(outer_len as usize, raw.len() - 4, "outer BE length must be filesize - 4");
    assert_eq!(inner_len as usize, raw.len() - 8, "inner LE length must be filesize - 8");
    raw[8..].to_vec()
}

fn load_payload(file: &str) -> Vec<u8> {
    let path = format!("{FIXTURE_DIR}/{file}");
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read fixture {path}: {e}"));
    strip_dat_framing(&raw)
}

fn restore_non_replicated_state(core: &mut ExchangeCore) {
    core.ssp.rebuild_spot_pair_index();
    for up in core.ups.users.values_mut() {
        for pos in up.positions.values_mut() {
            pos.adl_eligibility = if pos.margin_mode == MarginMode::Isolated { 100 } else { 0 };
            pos.pending_adl_size = 0;
            pos.liquidation_flow = None;
        }
    }
    let ups = core.ups.clone();
    let ssp = core.ssp.clone();
    core.risk.liquidation_engine.rebuild_indices(&ups, &ssp);
}

fn recover_from_java_dat() -> ExchangeCore {
    let mut core = ExchangeCore::new();

    let re_payload = load_payload("snapshot_88888_RE_0.dat");
    read_risk_engine_payload(&re_payload, &mut core)
        .expect("Java RiskEngine .dat payload must parse with the Rust RE parser");

    let me_payload = load_payload("snapshot_88888_ME_0.dat");
    core.matching = MatchingEngineRouter::chronicle_read(&mut ChronicleReader::new(&me_payload))
        .expect("Java MatchingEngineRouter .dat payload must parse with the Rust ME parser");

    restore_non_replicated_state(&mut core);
    core
}

#[test]
fn java_risk_engine_dat_payload_parses_with_rust_parser() {
    let re_payload = load_payload("snapshot_88888_RE_0.dat");
    let mut core = ExchangeCore::new();
    let result = read_risk_engine_payload(&re_payload, &mut core);
    assert!(result.is_ok(), "RE parser returned error: {:?}", result.err());
}

#[test]
fn java_matching_engine_dat_payload_parses_with_rust_parser() {
    let me_payload = load_payload("snapshot_88888_ME_0.dat");
    let result = MatchingEngineRouter::chronicle_read(&mut ChronicleReader::new(&me_payload));
    assert!(result.is_ok(), "ME parser returned error: {:?}", result.err());
}

#[test]
fn recovered_java_snapshot_conserves_global_balance() {
    let core = recover_from_java_dat();
    let report = core.query_total_balance();
    assert!(
        report.is_global_zero(),
        "recovered Java snapshot must be globally conserved; report = {report:?}"
    );
}

#[test]
fn recovered_java_snapshot_has_expected_currencies_and_symbols() {
    let core = recover_from_java_dat();
    for cur in [USD, USDT, BTC, ETH] {
        assert!(core.ssp.currencies.contains_key(&cur), "currency {cur} must exist");
    }
    for sym in [SPOT_BTC_USDT, SPOT_ETH_USDT, PERP_BTC] {
        assert!(core.ssp.symbols.contains_key(&sym), "symbol {sym} must exist");
    }
    let btc = core.ssp.symbols.get(&SPOT_BTC_USDT).unwrap();
    assert_eq!(btc.base_currency, BTC);
    assert_eq!(btc.quote_currency, USDT);
    let perp = core.ssp.symbols.get(&PERP_BTC).unwrap();
    assert_eq!(perp.quote_currency, USD, "BTC-PERP settles in USD");
}

#[test]
fn recovered_java_snapshot_has_expected_mark_prices() {
    let core = recover_from_java_dat();
    let mark_perp = core.risk.last_price_cache.get(&PERP_BTC).map(|r| r.mark_price);
    assert_eq!(mark_perp, Some(1000), "mark(200) must be 1000");
    let mark_spot = core.risk.last_price_cache.get(&SPOT_BTC_USDT).map(|r| r.mark_price);
    assert_eq!(mark_spot, Some(50000), "mark(100) must be 50000");
}

#[test]
fn recovered_java_snapshot_has_expected_users() {
    let core = recover_from_java_dat();
    for uid in [U10, U11, U12, U13, U20, U22] {
        assert!(core.ups.get(uid).is_some(), "user {uid} must exist");
    }
}

#[test]
fn recovered_java_snapshot_spot_balances_and_locks() {
    let core = recover_from_java_dat();

    let u10 = core.ups.get(U10).unwrap();
    assert_eq!(u10.account(USD), 100_000, "u10 deposited USD 100000");
    assert!(u10.account(USDT) > 0, "u10 still holds USDT after buying 10 BTC");

    let u11 = core.ups.get(U11).unwrap();
    assert_eq!(u11.locked(BTC), 5, "u11 resting ask 5@60000 locks 5 BTC");

    let u12 = core.ups.get(U12).unwrap();
    assert_eq!(u12.locked(ETH), 20, "u12 resting ask 20@3000 on sym101 locks 20 ETH");
}

#[test]
fn recovered_java_snapshot_futures_positions() {
    let core = recover_from_java_dat();

    let u10_long = core
        .ups
        .get(U10)
        .unwrap()
        .positions
        .values()
        .find(|p| p.symbol == PERP_BTC && p.direction == PositionDirection::Long)
        .expect("u10 must hold a LONG position on sym200");
    assert_eq!(u10_long.open_volume, 3, "u10 LONG 3 contracts");
    assert_eq!(u10_long.margin_mode, MarginMode::Isolated);

    let u13_short = core
        .ups
        .get(U13)
        .unwrap()
        .positions
        .values()
        .find(|p| p.symbol == PERP_BTC && p.direction == PositionDirection::Short)
        .expect("u13 must hold a SHORT position on sym200");
    assert_eq!(u13_short.open_volume, 3, "u13 SHORT 3 contracts");
}

#[test]
fn recovered_java_snapshot_order_books_have_resting_orders() {
    let core = recover_from_java_dat();

    let u11_ask: i64 = core
        .matching
        .user_orders(U11)
        .into_iter()
        .filter(|(sym, o)| *sym == SPOT_BTC_USDT && o.price == 60000)
        .map(|(_, o)| o.size - o.filled)
        .sum();
    assert_eq!(u11_ask, 5, "sym100 must have u11 resting ask 5@60000");

    let u12_ask: i64 = core
        .matching
        .user_orders(U12)
        .into_iter()
        .filter(|(sym, o)| *sym == SPOT_ETH_USDT && o.price == 3000)
        .map(|(_, o)| o.size - o.filled)
        .sum();
    assert_eq!(u12_ask, 20, "sym101 must have u12 resting ask 20@3000");
}

#[test]
fn recovered_java_snapshot_loans_survive() {
    let core = recover_from_java_dat();

    let u20 = core.ups.get(U20).unwrap();
    assert!(u20.isolated_loans.contains_key(&ISO_LOAN_ID), "isolated loan 9001 must exist on u20");

    let u22 = core.ups.get(U22).unwrap();
    assert!(u22.cross_loans.contains_key(&CROSS_LOAN_ID), "cross loan 9002 must exist on u22");

    assert!(
        core.risk.loan_service.loan_pool_available.get(&USDT).copied().unwrap_or(0) > 0,
        "USDT loan pool must retain funds after two 1-unit draws"
    );
}
