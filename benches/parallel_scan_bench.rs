// 单命令停顿基准:funding 与强平现在都用强平引擎的 symbol -> holders 索引(只扫持仓用户)。
// 本 bench 测在 TOTAL 个账户中、某 symbol 有 `holders` 个持仓时,一条 SETTLE_FUNDINGFEES /
// 一条 targeted LiquidationScan 占用单管线(= 阻塞后续所有外部命令,如撮合/下单)的墙钟时长。
// 串行(workers=1,生产默认)——索引扫只碰 holders,与 TOTAL 无关。
//
// 跑:cargo bench --bench parallel_scan_bench
use std::collections::BTreeSet;
use std::hint::black_box;
use std::io::Write;
use std::time::{Duration, Instant};

use exchange_core_rs::core::common::cmd::order_command::OrderCommand;
use exchange_core_rs::core::common::cmd::order_command_type::OrderCommandType;
use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
use exchange_core_rs::core::common::margin_mode::MarginMode;
use exchange_core_rs::core::common::order_action::OrderAction;
use exchange_core_rs::core::common::position_direction::PositionDirection;
use exchange_core_rs::core::common::symbol_position_record::SymbolPositionRecord;
use exchange_core_rs::core::common::symbol_type::SymbolType;
use exchange_core_rs::core::exchange_api::ExchangeApi;
use exchange_core_rs::core::processors::parallel::ComputeConfig;

const BASE: i32 = 1;
const QUOTE: i32 = 2;
const SYMBOL: i32 = 900;
const MARK: i64 = 100;
const RATE: i64 = 100;
const RATE_SCALE_K: i64 = 1_000;
const OPEN_VOLUME: i64 = 10;

// TOTAL accounts = holders here, to match the Java bench's account count for a fair
// funding comparison (the real system has far more idle accounts; with Rust's BTreeMap ups
// that makes ups.get O(log total), which the 20M run showed).
const HOLDER_COUNTS: &[usize] = &[1_000, 10_000, 100_000];

fn futures_spec() -> CoreSymbolSpecification {
    CoreSymbolSpecification {
        symbol_id: SYMBOL,
        symbol_type: SymbolType::FuturesContractPerpetual,
        base_currency: BASE,
        quote_currency: QUOTE,
        base_scale_k: 1,
        quote_scale_k: 1,
        ..Default::default()
    }
}

// TOTAL Active users; the first `holders` hold a healthy SYMBOL position (open at mark ->
// zero pnl, margin present -> the liquidation scan finds nothing to liquidate). The
// symbol_to_users index is populated to match a real position-open flow.
fn build(total: usize, holders: usize) -> ExchangeApi {
    let mut api = ExchangeApi::new();
    api.add_currency(BASE, 1);
    api.add_currency(QUOTE, 1);
    api.add_futures_symbol(futures_spec());
    api.set_mark_price(SYMBOL, MARK);

    let ups = &mut api.core().ups;
    for uid in 1..=total as i64 {
        ups.add_empty_user_profile(uid);
        if (uid as usize) <= holders {
            let mut pos = SymbolPositionRecord::new(uid, SYMBOL, QUOTE, MarginMode::Isolated, 1);
            pos.direction = if uid % 2 == 0 { PositionDirection::Long } else { PositionDirection::Short };
            pos.open_volume = OPEN_VOLUME;
            pos.open_price_sum = OPEN_VOLUME * MARK;
            pos.open_init_margin_sum = OPEN_VOLUME * MARK;
            ups.get_mut(uid).unwrap().positions.insert(SYMBOL, pos);
        }
    }
    api.core().risk.liquidation_engine.symbol_to_users.insert(SYMBOL, (1..=holders as i64).collect::<BTreeSet<_>>());
    api.enable_liquidation();
    api
}

fn funding_cmd() -> OrderCommand {
    OrderCommand {
        command: OrderCommandType::SettleFundingfees,
        symbol: SYMBOL,
        action: Some(OrderAction::Bid),
        price: RATE,
        size: RATE_SCALE_K,
        ..Default::default()
    }
}

fn liquidation_cmd() -> OrderCommand {
    // symbol >= 0 -> targeted scan via the symbol_to_users index (scans only holders).
    OrderCommand { command: OrderCommandType::LiquidationScan, symbol: SYMBOL, timestamp: 1, ..Default::default() }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn time_cmd(api: &mut ExchangeApi, make: fn() -> OrderCommand) -> f64 {
    (0..3)
        .map(|_| {
            let mut cmd = make();
            let t0 = Instant::now();
            api.core().process_command(&mut cmd);
            let dt = t0.elapsed();
            black_box(&cmd.fund_events);
            ms(dt)
        })
        .fold(f64::MAX, f64::min)
}

fn main() {
    // How long a single funding / liquidation command stalls the pipeline (blocking all other
    // commands: matching, order placement) at a realistic 20M total accounts, by how many of
    // them hold the settled/scanned symbol. workers=1 is the production default (serial single
    // pipeline); workers=8 shows the ceiling with internal parallelism turned on.
    const TOTAL: usize = 20_000_000;
    let holders_list = [1_000usize, 10_000, 100_000, 1_000_000];
    let worker_counts = [1usize, 4, 8];
    println!("stall = time one command blocks the single pipeline; TOTAL={TOTAL} accounts\n");
    println!("{:>10}  {:>8}  {:>18}  {:>20}", "holders", "workers", "funding stall ms", "liquidation stall ms");

    for &holders in &holders_list {
        eprintln!("building total={TOTAL} holders={holders}...");
        let mut api = build(TOTAL, holders);
        for &w in &worker_counts {
            api.core().risk.set_compute_config(ComputeConfig { workers: w, serial_threshold: 0 });
            let funding = time_cmd(&mut api, funding_cmd);
            let liq = time_cmd(&mut api, liquidation_cmd);
            println!("{holders:>10}  {w:>8}  {funding:>18.2}  {liq:>20.2}");
            std::io::stdout().flush().ok();
        }
        drop(api);
    }

    println!(
        "\nStall tracks holders, not total accounts. workers>1 only compresses the scan; the\n\
         serial merge + per-holder apply dominate, so it barely helps."
    );
}
