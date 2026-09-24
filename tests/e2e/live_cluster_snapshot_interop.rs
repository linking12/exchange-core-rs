use exchange_core_rs::core::common::margin_mode::MarginMode;
use exchange_core_rs::core::exchange_core::ExchangeCore;
use exchange_core_rs::core::processors::matching_engine_router::MatchingEngineRouter;
use exchange_core_rs::core::processors::risk_engine::read_risk_engine_payload;
use exchange_core_rs::core::snapshot::chronicle_reader::ChronicleReader;
use exchange_core_rs::core::snapshot::marshalling::ChronicleMarshallable;
use exchange_core_rs::core::snapshot::module_frame::decode_module_payload;

const RE_SHARDS: [&str; 2] = ["live_re_0.dat", "live_re_1.dat"];
const ME_SHARDS: [&str; 4] = ["live_me_0.dat", "live_me_1.dat", "live_me_2.dat", "live_me_3.dat"];

fn fixture_path(file: &str) -> String {
    format!("{}/tests/snapshot_fixtures/{}", env!("CARGO_MANIFEST_DIR"), file)
}

fn load_payload(file: &str) -> Vec<u8> {
    let path = fixture_path(file);
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read fixture {path}: {e}"));
    decode_module_payload(&raw)
        .unwrap_or_else(|e| panic!("decode_module_payload failed for {file}: {e}"))
}

fn recover_re_shard(file: &str) -> ExchangeCore {
    let payload = load_payload(file);
    let mut core = ExchangeCore::new();
    read_risk_engine_payload(&payload, &mut core)
        .unwrap_or_else(|e| panic!("RE parser failed for live shard {file}: {e:?}"));
    core
}

fn recover_me_shard(file: &str) -> MatchingEngineRouter {
    let payload = load_payload(file);
    MatchingEngineRouter::chronicle_read(&mut ChronicleReader::new(&payload))
        .unwrap_or_else(|e| panic!("ME parser failed for live shard {file}: {e:?}"))
}

#[test]
fn live_cluster_all_six_shards_parse() {
    for file in RE_SHARDS {
        let payload = load_payload(file);
        let mut core = ExchangeCore::new();
        let result = read_risk_engine_payload(&payload, &mut core);
        assert!(result.is_ok(), "RE shard {file} failed to parse: {:?}", result.err());
    }
    for file in ME_SHARDS {
        let payload = load_payload(file);
        let result = MatchingEngineRouter::chronicle_read(&mut ChronicleReader::new(&payload));
        assert!(result.is_ok(), "ME shard {file} failed to parse: {:?}", result.err());
    }
}

#[test]
fn live_cluster_re_shards_have_nonempty_state() {
    let cores: Vec<ExchangeCore> = RE_SHARDS.iter().map(|f| recover_re_shard(f)).collect();

    let total_users: usize = cores.iter().map(|c| c.ups.users.len()).sum();
    assert!(total_users > 0, "combined RE shards must contain at least one user");

    let users_with_balance = cores
        .iter()
        .flat_map(|c| c.ups.users.values())
        .filter(|u| u.accounts.values().any(|&bal| bal != 0))
        .count();
    assert!(users_with_balance > 0, "at least one user must hold a non-zero account balance");

    let futures_positions = cores
        .iter()
        .flat_map(|c| c.ups.users.values())
        .flat_map(|u| u.positions.values())
        .filter(|p| p.open_volume > 0)
        .count();

    let loans = cores
        .iter()
        .flat_map(|c| c.ups.users.values())
        .map(|u| u.isolated_loans.len() + u.cross_loans.len())
        .sum::<usize>();

    assert!(
        futures_positions > 0 || loans > 0,
        "futures/loan E2E must leave at least one open position or loan (positions={futures_positions}, loans={loans})"
    );

    for margin_mode in cores
        .iter()
        .flat_map(|c| c.ups.users.values())
        .flat_map(|u| u.positions.values())
        .map(|p| p.margin_mode)
    {
        assert!(
            matches!(margin_mode, MarginMode::Isolated | MarginMode::Cross),
            "position margin_mode must decode to a valid variant"
        );
    }

    let symbols: usize = cores.iter().map(|c| c.ssp.symbols.len()).sum();
    assert!(symbols > 0, "RE shards must carry symbol specifications");
}

#[test]
fn live_cluster_re_shard_user_partitions_are_disjoint() {
    let cores: Vec<ExchangeCore> = RE_SHARDS.iter().map(|f| recover_re_shard(f)).collect();
    let mut seen = std::collections::BTreeSet::new();
    for core in &cores {
        for &uid in core.ups.users.keys() {
            assert!(seen.insert(uid), "uid {uid} appears in more than one RE shard");
        }
    }
}

#[test]
fn live_cluster_me_shards_have_order_books() {
    let routers: Vec<MatchingEngineRouter> = ME_SHARDS.iter().map(|f| recover_me_shard(f)).collect();

    let empty_hash = MatchingEngineRouter::new().order_books_state_hash();
    let shards_with_books = routers
        .iter()
        .filter(|r| r.order_books_state_hash() != empty_hash)
        .count();
    assert!(
        shards_with_books > 0,
        "at least one ME shard must contain order books / symbol specs"
    );

    let uids: Vec<i64> = RE_SHARDS
        .iter()
        .map(|f| recover_re_shard(f))
        .flat_map(|c| c.ups.users.keys().copied().collect::<Vec<_>>())
        .collect();

    let resting_orders: usize = routers
        .iter()
        .flat_map(|r| uids.iter().map(move |&uid| r.user_orders(uid).len()))
        .sum();

    assert!(
        resting_orders > 0,
        "spot/mixed E2E must leave resting orders across the ME shards (found {resting_orders})"
    );
}
