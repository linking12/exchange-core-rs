# exchange-core-rs

[English](README.md) · **中文**

![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)
![edition](https://img.shields.io/badge/edition-2021-blue)
![status](https://img.shields.io/badge/status-alpha-yellow)

Rust 实现的确定性单线程撮合与风控引擎,移植自
[**linking12/raft-exchange**](https://github.com/linking12/raft-exchange) —— 一个基于 Raft 复制的
加密交易所内核(现货 · 永续/交割合约 · 杠杆 · 借贷 · 统一账户 UTA)。它被设计为 Raft 共识层背后的状态机:
每个节点按同一串有序命令 apply,得到逐字节一致的状态。

> `raft-exchange`(Java)在经典 exchange-core 架构之上,把 JRaft 共识与 Disruptor 执行流水线结合。
> 本 crate 用 Rust 重写该引擎,并把**多处理器 Disruptor 流水线塌缩为单线程确定性管线**,以便直接由
> Raft 日志驱动。

---

## 特性

- **现货 + 衍生品** —— 现货撮合、永续与交割合约、逐仓/全仓保证金、借贷池、资金费、强平 / ADL / 保险基金、交割结算。
- **天生确定性** —— 单线程、单条有序管线(`R1 → ME → R2 → 排空`)。所有影响输出的迭代都走 `BTreeMap` / 显式排序,
  **禁用 `HashMap` 迭代序**。
- **定点金额** —— 金额用 `i64`,中间计算用 `i128` 防溢出。
- **面向 Raft** —— 可插拔的命令提交器与快照(`persist` / `recover`)钩子,供 install-snapshot;不内嵌线程或传输层。
- **零开销日志** —— 仅 `log` 门面;未装 backend 时 `trace!`/`debug!` 宏短路。

---

## 安装

本 crate 尚未发布到 crates.io,请以 path 或 git 方式依赖。

```toml
[dependencies]
exchange-core-rs = { git = "https://github.com/linking12/raft-exchange" }
# 或
exchange-core-rs = { path = "../exchange-core-rs" }
```

MSRV:Rust 1.75(edition 2021)。

---

## 快速开始

`ExchangeApi` 持有一个 `ExchangeCore`。构造后经 `api.core()` 挂事件回调,再按 配置 → 交易 → 查询 使用。

```rust
use exchange_core_rs::core::exchange_api::{ExchangeApi, PlaceOrderRequest};
use exchange_core_rs::core::common::core_symbol_specification::CoreSymbolSpecification;
use exchange_core_rs::core::common::symbol_type::SymbolType;
use exchange_core_rs::core::common::order_action::OrderAction;
use exchange_core_rs::core::common::order_type::OrderType;
use exchange_core_rs::core::trade_events_handler::{
    TradeEventsHandler, OrderBook, SpotExecutionReport, FuturesExecutionReport,
};
use exchange_core_rs::core::fund_events_handler::{FundEventsHandler, FundEventReport};

// 外部只实现两个 handler trait;例如 Raft server 把事件吐到 Kafka。
struct MyTradeHandler;
impl TradeEventsHandler for MyTradeHandler {
    fn order_book(&mut self, _ob: OrderBook) {}
    fn spot_execution_report(&mut self, _r: SpotExecutionReport) {}
    fn futures_execution_report(&mut self, _r: FuturesExecutionReport) {}
}
struct MyFundHandler;
impl FundEventsHandler for MyFundHandler {
    fn fund_event_report(&mut self, _r: FundEventReport) {}
}

let mut api = ExchangeApi::new();
api.core().with_events_handlers(MyTradeHandler, MyFundHandler);

// 配置:货币(+精度)、symbol、开户、充值。
api.add_currency(1, 1);
api.add_currency(2, 1);
api.add_symbol(CoreSymbolSpecification {
    symbol_id: 100, symbol_type: SymbolType::CurrencyExchangePair,
    base_currency: 1, quote_currency: 2, base_scale_k: 1, quote_scale_k: 1,
    taker_fee: 2, maker_fee: 1, ..Default::default()
});
api.add_user(1);
api.add_user(2);
api.balance_adjustment(1, 1, 1_000_000, 1);
api.balance_adjustment(2, 2, 10_000_000, 2);

// 交易(现货)。买单必须给 reserve_bid_price。
api.place_order(PlaceOrderRequest { order_id: 5001, uid: 1, symbol: 100, price: 20_000, size: 1,
    reserve_bid_price: 0, action: OrderAction::Ask, order_type: OrderType::Gtc });
api.place_order(PlaceOrderRequest { order_id: 5002, uid: 2, symbol: 100, price: 20_000, size: 1,
    reserve_bid_price: 20_000, action: OrderAction::Bid, order_type: OrderType::Gtc });

// 查询 / 报表(只读)。
assert!(api.total_balance().is_global_zero()); // 全局账面净零(强不变量)
```

API 方法按域(现货 / 期货 / 借贷)和层次(配置 → 交易 → 查询 → 报表)分组。期货与借贷有专用请求类型
(`PlaceFuturesOrderRequest`、`ClosePositionRequest`、`loan_create`、`pool_deposit` 等);冷门命令走通用入口
`api.submit(OrderCommand)`。

---

## 架构

### 确定性管线

每条命令都流过同一条同步管线。强平不是后台线程:markprice 更新或 `LIQUIDATION_SCAN` 在 R1 做仓位检查,
经*命令提交器*把强平命令入队,`drive_pending` 再把它们当普通命令重放,直到队列排空 —— 全部在一次
`process_command` 内闭合。

```text
process_command(cmd):
    apply_one(cmd):
        R1  risk.pre_process_command    // 校验 / 冻结 / 仓位预处理 / 扫描
        ME  matching.process_order      // 撮合(下单/撤单/改单/减量/强平吃单)
        R2  risk.handler_risk_release   // 结算 / 释放 / PnL / 费用 / 产出事件
        └─ results_consumer(cmd, seq…)  // 把结果与事件流给下游(若已注入)
    drive_pending():                    // 重放级联的 FORCE→IF→ADL / loan 强平命令
                                        // 直到 pending 队列排空
```

### 模块划分

`raft-exchange` 的引擎是单一 Java module,故 Rust 侧也是**单 crate**,在 `src/core/` 下按域用 `mod` 划分。

| 模块 | 职责 |
|------|------|
| `core::common` | 领域模型 —— `OrderCommand`、`CoreSymbolSpecification`、`UserProfile`、`Order`、`FundEvent` 等 |
| `core::common::cmd` | 命令类型 `OrderCommandType` + 结果码 `CommandResultCode` |
| `core::orderbook` | 订单簿:`IOrderBook` + Direct(O(log N))/ Naive 两实现 |
| `core::processors` | `RiskEngine`、`MatchingEngineRouter`、资金费、ADL |
| `core::processors::loan` | 借贷:命令分派、清算引擎、利率曲线 |
| `core::processors::liquidation` | 强平引擎、破产价、扫描调度器 |
| `core::exchange_core` | `ExchangeCore` 编排(管线 + 快照) |
| `core::exchange_api` | `ExchangeApi` 高层门面 |
| `core::reports` | 报表:全局余额守恒、单用户、保险基金 |
| `core::utils` | 定点算术(`i128` 中间量、缩放) |

---

## 接入 Raft

引擎级操作(级联去向、周期扫描、快照)都经 `api.core()`。`with_command_submitter` 收一个实现
`CommandSubmitter` 的共享实例 —— 集群下把级联命令交给 Raft 复制,而非就地 apply。

```rust
use std::{cell::RefCell, collections::VecDeque, rc::Rc};
use exchange_core_rs::core::common::cmd::order_command::OrderCommand;
use exchange_core_rs::core::processors::liquidation::command_submitter::CommandSubmitter;

struct RaftSubmitter { queue: Rc<RefCell<VecDeque<OrderCommand>>> }
impl CommandSubmitter for RaftSubmitter {
    fn submit(&mut self, cmd: OrderCommand) { self.queue.borrow_mut().push_back(cmd); } // 实际:raft.propose(cmd)
}

let queue: Rc<RefCell<VecDeque<OrderCommand>>> = Rc::new(RefCell::new(VecDeque::new()));
api.core().with_command_submitter(Rc::new(RefCell::new(RaftSubmitter { queue: queue.clone() })));

// 周期扫描(leader 时钟)+ 快照(install-snapshot;RE / ME / EC 模块)。
api.core().tick_liquidation_scheduler(now);
api.core().persist(snapshot_id, instance_id);
api.core().recover(snapshot_id, instance_id);
```

快照走 **Chronicle Wire** 二进制分帧(与 Java `.ecs`/`.dat` 快照互通,RE/ME 模块 + LZ4 自动探测)。
另有独立的 `EC` 模块存放 Rust 侧计数器(如结果序列号)—— Java 侧不持久化这些;该模块按可选读取,故 Java/旧快照也能干净恢复。

---

## 构建与测试

```bash
cargo build --release

cargo test --lib                 # 与生产代码同文件的单元测试
cargo test --test e2e            # 引擎级 e2e + 守恒 proptest
cargo test --test integration    # 对 Java oracle 的对拍(仅公开 API)
cargo test --test conformance    # 黄金向量对拍(需先用 Java 生成 golden)
cargo test --test orderbook_diff # Direct vs Naive 订单簿差分
cargo test                       # 全部

cargo bench --bench engine_throughput  # 纯引擎吞吐基准(绕开 Raft)
```

测试布局:`src/` 只留与生产代码同文件的单元测试;独立测试都在 `tests/` 下,只用公开 API。

---

## 与 raft-exchange 的关系

本 crate 移植的是 Java 项目的**撮合与风控引擎**。它有意**不**重写 Disruptor 异步提交层、JRaft/Aeron 传输、
`groupingControl` —— 那些属于外层 server。引擎以纯库形式被消费,由 Raft 状态机驱动。

与 Java 实现的行为等价性由分层策略保障(IT 翻译对拍 / 守恒 proptest / 对 Java oracle 的黄金向量对拍)加差分模糊,
详见 [`CONSISTENCY.md`](CONSISTENCY.md)。

---

## 许可

本仓库尚未指定许可协议。许可以上游 [linking12/raft-exchange](https://github.com/linking12/raft-exchange)
项目为准 —— 分发前请补上 `LICENSE` 文件。
