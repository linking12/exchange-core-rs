# 引擎吞吐基准规格(Java↔Rust 同口径复刻)

本文规定 `benches/engine_throughput.rs` 的**命令流与测量方法**,以便 Java `exchange-core` 侧用**同一口径**跑出可对比的数字。目标是量**纯引擎单命令处理成本**,不含共识/网络。

跑 Rust 侧:`cargo bench --bench engine_throughput`(release + `lto=true` + `codegen-units=1`)。

---

## 1. 测量边界(关键:两侧必须一致)

- Rust 测的是 **`ExchangeCore::process_command`**:单线程内联跑完 **R1(风控)→ ME(撮合)→ R2(结算)** 一整条,再回到调用方。**绕开 Raft/网络/序列化**。
- Java `exchange-core` 默认是 **Disruptor 多处理器**(R1/ME/R2 分处理器、跨核流水)。直接比"端到端吞吐"不公平(Java 靠流水并行、Rust 是单线程)。**同口径二选一**:
  - **(推荐)单线程直调**:在 Java 侧构造单分片、绕过 Disruptor,直接顺序调用等价的 R1→ME→R2 逻辑,测单命令成本;
  - 或 **端到端稳态**:测整条 Disruptor 的稳态吞吐,但须**明确标注**这是"多核流水 vs 单线程",不是同口径。
- 两侧都要:**预热**(见 §4)、用单调时钟计时一个紧循环、对结果做 `black_box`/等价手段防止死代码消除(DCE)。

## 2. 初始化状态(seeded state,两侧相同)

| 项 | 值 |
|---|---|
| symbol | id=`1`,类型 `CurrencyExchangePair`(现货对) |
| base / quote 币种 | base=`10`,quote=`20`;`currency_scale_k=1` |
| symbol scale | `base_scale_k=1`,`quote_scale_k=1` |
| 手续费 | `taker_fee=0`,`maker_fee=0`,`fee_scale_k=0`(**零费**,排除费算术噪声) |
| 用户 | BUYER uid=`1`,SELLER uid=`2` |
| 余额 | BUYER: QUOTE = `i64::MAX/4`;SELLER: BASE = `i64::MAX/4`(充足,永不 NSF) |
| 订单簿实现 | **Direct**(生产实现);Naive 仅作参考、不用于对比结论 |

所有订单 `size=1`;BID 的 `reserve_bid_price = price`,ASK 的 `reserve_bid_price = 0`;`order_id` 见各场景。

## 3. 各子基准的命令流

> 记号:`PLACE(oid, uid, side, type, price, size)`、`CANCEL(oid, uid)`。第 `i` 次迭代 `i` 从 0 起。

### A · place-only(纯下单,只增长)
- 循环:`PLACE(i+1, BUYER, BID, GTC, price=1+(i%2000), size=1)`。
- 只挂不撮(簿上无 ASK),簿单调增长;测挂单快路径。

### C · place+cancel(挂撤 churn)
- 每次迭代两条:`PLACE(i+1, BUYER, BID, GTC, price=1, size=1)` 然后 `CANCEL(i+1, BUYER)`。
- 簿基本恒空;测"挂+撤"往返。计一次迭代 = 一挂一撤(报 ns/op 时按迭代数,不是命令数——两侧口径要一致)。

### B1 · match DEEP(单桶 N 深)
- 预置:`for i in 0..N`:`PLACE(i+1, SELLER, ASK, GTC, price=100, size=1)`——**N 个 ASK 全在同一价 100**(单桶、N 深)。
- 测量:`N` 条 `PLACE(taker_oid, BUYER, BID, IOC, price=100, size=1)`,每条吃掉一个 ASK(taker_oid 从 N+1 起递增,保证唯一)。

### B2 · match WIDE(N 个价档)
- 预置:`for i in 0..N`:`PLACE(i+1, SELLER, ASK, GTC, price=100+i, size=1)`——**N 个 ASK 各占一价档**。
- 测量:`N` 条 `PLACE(taker_oid, BUYER, BID, IOC, price=100+N, size=1)`,每条吃掉当前最优。

### ME-only(纯订单簿对照,不走风控)
- 与 B1/B2 相同的命令流,但**直接调订单簿 `new_order`**(不经 R1/R2)。分别对 Direct 与 Naive 各跑 WIDE 与 DEEP。
- Java 侧对应:直接调 `IOrderBook.newOrder`(`OrderBookDirectImpl`)。Naive 仅参考。

## 4. 迭代数与预热

- **A / C**:先各跑 `50_000` 预热(不计),再跑 `1_000_000` 正式计时。
- **B(DEEP/WIDE)与 ME-only**:`N ∈ {5_000, 10_000, 20_000, 40_000}` 各测一次(看 ns/op 随 N 的斜率判断是否 O(N))。
- **Java 特别注意**:JIT(C2)需要足够预热才进稳态——A/C 的 1M 循环足够,但 B/ME 的小 N 可能没热身充分,Java 侧建议对每个 N **先跑几轮丢弃**再计时,否则 Java 会因未 JIT 而偏慢、失真。

## 5. 报告口径

每个场景输出:`iters`、总耗时(ms)、`ops/sec`、`ns/op`。
- Rust 结果(本机 macOS,单线程,绕开共识,仅量级参考):A/C ~130–145 ns/op(~7M ops/s);B 撮合 DEEP ~250 ns/op、WIDE ~300 ns/op;ME-only Direct ~70–90 ns/op 且随深度基本恒定,Naive O(N) 恶化。
- **建议 Java 侧额外报延迟分位(p50/p99/p99.9)**:Rust 无 GC,尾延迟是它的主要优势,只比中位吞吐会掩盖这点。

## 5b. Java 对手盘实测(2026-09-29,单线程直调 R1→ME→R2,绕开 Disruptor)

同机 macOS。Java = JDK25 Corretto、C2 已充分预热(每路径先跑丢弃)、`-Xms2g -Xmx2g`;单线程顺序调 `RiskEngine.preProcessCommand` → `MatchingEngineRouter.processOrder` → `RiskEngine.handlerRiskRelease`(=Disruptor 三处理器的等价业务序,seeded 状态同 §2)。Rust = release+LTO。ns/op,越小越快:

| 场景 | Rust | Java | 可比? |
|------|-----:|-----:|:--:|
| ME-only Direct DEEP | 71–74 | **16–17** | ✅ 纯撮合,两侧都直调 `newOrder` |
| ME-only Direct WIDE | 80–85 | **22–27** | ✅ 同上(Java 偶有 GC 抖动点) |
| C 挂+撤(空簿) | 133 | **79** | ✅ 空簿 → 快照 O(1) |
| B1 撮合 DEEP(单桶) | 240–263 | **54–65** | ✅ 单档 → 快照 O(1) |
| A 纯下单(增长) | 139 | 4258 ⚠️ | ❌ 见下 |
| B2 撮合 WIDE(N 档) | 286–294 | 4867→31872 ⚠️ | ❌ 见下 |
| ME-only Naive | O(N) 恶化 | O(N) 恶化 | ✅ 均仅参考 |

**⚠️ A / B2 不可直接比——口径 artifact,非撮合快慢**:Java `MatchingEngineRouter.processMatchingCommand`(`MatchingEngineRouter.java:258`)在**每条成功命令后无条件** `getL2MarketDataSnapshot(Integer.MAX_VALUE)`,成本 = O(当前价档数)。而 Rust `matching_engine_router.rs` 的 `process_order` **只在 `OrderBookRequest` 才 `fill_l2`**,下单/撤单/撮合路径不快照。于是簿档多时(A ≤2000 档、B2 至多 N 档)Java 被全深度快照拖成 O(档数);簿浅时(B1 单档、C 空簿)不受影响、数字真实。GC 全程仅 ~252ms,已排除 GC 主导(A 一项就 4258ms)。这条移植差异挂 CONSISTENCY.md §11 待复审。

**结论**:在**纯撮合与恒定簿深**的命令上,成熟 JIT 的 exchange-core2(Java)与 Rust 端在同量级、甚至更快(ME-only Direct 16ns vs 71ns;B1 65ns vs 246ns;C 79ns vs 133ns)。Rust 端在 A/B2 上"看起来快",只是因为它省掉了 Java 每命令全深度快照。要在 A/B2 上真可比,须两侧统一"是否每命令产 marketData"。

## 6. 公平性 checklist(避免比出假结论)
- [ ] 同机器、同 CPU 频率策略(关掉 turbo 抖动/降频更稳)。
- [ ] 测量边界一致(单线程直调 R1→ME→R2,或都标注为端到端流水)。
- [ ] 同 symbol/scale/零费/同价量分布/同 order_id 方案(见 §2/§3)。
- [ ] 都用生产订单簿(Direct);Naive 不进对比结论。
- [ ] **每命令 marketData 口径一致**:Java 每条命令产全深度 L2 快照(`MatchingEngineRouter.java:258`)、Rust 只在 `OrderBookRequest` 产;比 A / B2(簿档多)前须统一,否则测的是快照成本不是撮合(见 §5b)。
- [ ] 充分预热(尤其 Java JIT);对结果防 DCE。
- [ ] 除吞吐外报尾延迟分位。
- [ ] 明确都是**绕开共识**的引擎裸上限;真实系统吞吐由 Raft/BFT 共识层封顶(低一两个数量级)。
