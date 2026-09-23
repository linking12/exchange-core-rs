// 并行批量扫描基准:量化 funding 结算的墙钟耗时,并把一条 SETTLE_FUNDINGFEES 命令拆开计时,
// 回答「并行省多少」+「会不会堵住」。
//
// 关键真实性:funding 只结算**一个 symbol**,但它的扫描(map_users)遍历**全部 Active 账户**
// (对无该 symbol 仓位的用户返回 None),而 merge/apply 只处理**实际持有该 symbol 仓位**的用户。
// 所以本 bench 固定总账户数 = TOTAL,只让 HOLDERS 个子集持仓,分别计时:
//   - map scan (全量 O(TOTAL) 扫描,与持仓数无关) —— 1 worker vs 15 worker
//   - collect  (map + 串行 merge O(HOLDERS))
//   - apply    (串行写回 O(HOLDERS))
// 一条命令占用单管线的总停顿 ≈ collect + apply。
//
// 跑:cargo bench --bench parallel_scan_bench
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
use exchange_core_rs::core::processors::fundingfee_command_processor::FundingFeeCommandProcessor;
use exchange_core_rs::core::processors::parallel::ComputeConfig;
use exchange_core_rs::core::processors::twostep_command_processor::{TwoStepCommandProcessor, TwoStepContext};

const BASE: i32 = 1;
const QUOTE: i32 = 2;
const SYMBOL: i32 = 900;
const MARK: i64 = 100;
const RATE: i64 = 100;
const RATE_SCALE_K: i64 = 1_000;
const OPEN_VOLUME: i64 = 10;

const TOTAL: usize = 2_000_000;
// How many of the TOTAL accounts actually hold a position in SYMBOL (the rest are Active
// but flat -> scanned by map, ignored by merge/apply). Last row = every account holds one.
const HOLDER_COUNTS: &[usize] = &[10_000, 100_000, 1_000_000];

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

// TOTAL Active users; only the first `holders` hold a SYMBOL position (alternating
// Long/Short so half are payers, half receivers under the Bid funding action).
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
            // open at mark (price_sum = volume*mark) => zero unrealized pnl, and margin >= any
            // maintenance, so the liquidation scan sees HEALTHY positions (no liquidation flow
            // triggered). funding ignores these two fields.
            pos.open_price_sum = OPEN_VOLUME * MARK;
            pos.open_init_margin_sum = OPEN_VOLUME * MARK;
            ups.get_mut(uid).unwrap().positions.insert(SYMBOL, pos);
        }
    }
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

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn set_workers(api: &mut ExchangeApi, w: usize) {
    api.core().risk.set_compute_config(ComputeConfig { workers: w, serial_threshold: 0 });
}

// Full-scan map: map_users over ALL Active accounts with a light read-only closure.
fn time_map(api: &mut ExchangeApi) -> f64 {
    let core = api.core();
    let ctx = TwoStepContext::new(&mut core.risk, &mut core.ups, &core.ssp);
    let proc = FundingFeeCommandProcessor;
    let t0 = Instant::now();
    let out = proc.map_users(&ctx, |_| true, |u| u.positions.get(&SYMBOL).map(|p| p.open_volume).unwrap_or(0));
    let dt = t0.elapsed();
    black_box(out.len());
    ms(dt)
}

fn time_collect(api: &mut ExchangeApi) -> f64 {
    let core = api.core();
    let mut ctx = TwoStepContext::new(&mut core.risk, &mut core.ups, &core.ssp);
    let proc = FundingFeeCommandProcessor;
    let mut cmd = funding_cmd();
    let t0 = Instant::now();
    let rc = proc.collect(&mut ctx, &mut cmd);
    let dt = t0.elapsed();
    black_box(rc);
    black_box(&cmd.funding_fee_event);
    ms(dt)
}

// apply after a (untimed) collect that fills the command.
fn time_apply(api: &mut ExchangeApi) -> f64 {
    let core = api.core();
    let mut ctx = TwoStepContext::new(&mut core.risk, &mut core.ups, &core.ssp);
    let proc = FundingFeeCommandProcessor;
    let mut cmd = funding_cmd();
    proc.collect(&mut ctx, &mut cmd);
    let t0 = Instant::now();
    proc.apply(&mut ctx, &mut cmd);
    let dt = t0.elapsed();
    black_box(&cmd.fund_events);
    ms(dt)
}

fn main() {
    println!("cores=15  total accounts={TOTAL}  volume/user={OPEN_VOLUME}  mark={MARK}");
    println!("(map scans ALL Active accounts; merge/apply only touch the `holders` subset)\n");
    println!(
        "{:>12}  {:>12}  {:>12}  {:>13}  {:>14}  {:>16}",
        "holders", "map 1w ms", "map 15w ms", "collect15 ms", "apply ms", "stall15 ms"
    );

    for &holders in HOLDER_COUNTS {
        eprintln!("building total={TOTAL} holders={holders}...");
        let mut api = build(TOTAL, holders);

        set_workers(&mut api, 1);
        let map_1w = time_map(&mut api);
        set_workers(&mut api, 15);
        let map_15w = time_map(&mut api);
        let collect_15w = time_collect(&mut api);
        let apply = time_apply(&mut api);
        let stall = collect_15w + apply;

        println!(
            "{holders:>12}  {map_1w:>12.1}  {map_15w:>12.1}  {collect_15w:>13.1}  {apply:>14.1}  {stall:>16.1}"
        );
        std::io::stdout().flush().ok();
        drop(api);
    }

    // ---- isolated map-scan speedup vs worker count (min of 3 reps to damp scheduling noise) ----
    // Two data densities: `flat` = all accounts are Active-but-position-less (cheapest per-user
    // work), `full` = every account carries a SYMBOL position (heavier, more memory traffic).
    let matrix = [1usize, 2, 4, 8, 15];
    for (label, holders) in [("flat (all position-less)", 0usize)] {
        eprintln!("building scan-matrix {label}...");
        let mut api = build(TOTAL, holders);
        println!("\n== map-scan speedup vs workers, TOTAL={TOTAL}, {label} (min of 3) ==");
        println!("{:>10}  {:>12}  {:>10}", "workers", "map ms", "speedup");
        let mut base = 0.0f64;
        for (i, &w) in matrix.iter().enumerate() {
            set_workers(&mut api, w);
            let best = (0..3).map(|_| time_map(&mut api)).fold(f64::MAX, f64::min);
            if i == 0 {
                base = best;
            }
            println!("{w:>10}  {best:>12.1}  {:>9.2}x", base / best);
            std::io::stdout().flush().ok();
        }
        drop(api);
    }

    // ---- liquidation: symbol-index scan (targeted, like the real engine) vs full scan ----
    // Same engine, same data; the only difference is enumeration: targeted uses the
    // symbol_to_users index (scans only holders), full scans all TOTAL accounts. Both run
    // serial (workers=1) to isolate the enumeration cost, not parallelism. This is exactly
    // the saving funding would get by reusing the same index instead of scanning everyone.
    {
        use std::collections::BTreeSet;
        println!("\n== liquidation: symbol-index scan (targeted) vs full scan, TOTAL={TOTAL}, serial (min of 3) ==");
        println!("{:>10}  {:>16}  {:>14}  {:>10}", "holders", "index-scan ms", "full-scan ms", "saved");
        for &holders in &[10_000usize, 100_000, 1_000_000] {
            eprintln!("building liq total={TOTAL} holders={holders}...");
            let mut api = build(TOTAL, holders);
            // Populate the symbol->holders index (normally maintained on position open) and arm it.
            api.core().risk.liquidation_engine.symbol_to_users.insert(SYMBOL, (1..=holders as i64).collect::<BTreeSet<_>>());
            api.enable_liquidation();
            // serial (workers=1) so the only thing measured is enumeration range:
            // index -> holders vs full -> all TOTAL accounts, not parallelism.
            api.core().risk.set_compute_config(ComputeConfig { workers: 1, serial_threshold: usize::MAX });

            let targeted = (0..3)
                .map(|_| {
                    let mut cmd = OrderCommand { command: OrderCommandType::LiquidationScan, symbol: SYMBOL, timestamp: 1, ..Default::default() };
                    let t0 = Instant::now();
                    api.core().process_command(&mut cmd);
                    ms(t0.elapsed())
                })
                .fold(f64::MAX, f64::min);

            let full = (0..3)
                .map(|_| {
                    let mut cmd = OrderCommand { command: OrderCommandType::LiquidationScan, symbol: -1, size: 0, timestamp: 1, ..Default::default() };
                    let t0 = Instant::now();
                    api.core().process_command(&mut cmd);
                    ms(t0.elapsed())
                })
                .fold(f64::MAX, f64::min);

            println!("{holders:>10}  {targeted:>16.3}  {full:>14.1}  {:>9.0}x", full / targeted);
            std::io::stdout().flush().ok();
            drop(api);
        }
    }

    println!(
        "\nmap cost tracks total accounts (full scan), NOT holders; apply/merge track holders.\n\
         Parallelism only compresses the map scan; when holders is small the whole command is\n\
         cheap regardless of cores -- the O(total-accounts) full scan is the real fixed cost."
    );
}
