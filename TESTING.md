# 测试与脚本指南

本 crate 是单 crate 结构,测试目标全部由 cargo 自动发现(`Cargo.toml` 无显式 `[[test]]`)。
本文列出所有测试分类、运行命令、前置条件,以及 fixture 与脚本的来龙去脉。

> 本文只讲**怎么跑、文件在哪**。Java↔Rust 一致性对拍的**方法论**(三层递进防线 + 归一化规格 + 已发现的真实分歧)见 [CONSISTENCY.md](CONSISTENCY.md)。

## 测试分类与运行

| 目标 | 命令 | 覆盖 | 前置 |
|---|---|---|---|
| 单元测试 | `cargo test --lib` | 引擎各模块的进程内单测(含 `src/core/snapshot` 用 `include_bytes!` 读入库 fixture 的序列化 roundtrip) | 无 |
| 一致性对拍 | `cargo test --test conformance` | 回放入库的 `.stream` 命令流,逐行断言 == `.golden`(Java 导出的 oracle) | 默认读 `tests/conformance_vectors/`;可用 `CONFORMANCE_VECTORS_DIR` 覆盖 |
| e2e | `cargo test --test e2e` | 端到端场景(见下) | 无(全部读入库 fixture) |
| 集成(Java 对拍) | `cargo test --test integration` | `it_*` 一组与 Java exchange-core 行为对齐的集成用例 | 无 |
| 订单簿基线对拍 | `cargo test --test orderbook_base_parity` | Direct/Naive 订单簿与基线一致 | 无 |
| 订单簿差分 | `cargo test --test orderbook_diff` | 订单簿实现间差分 | 无 |
| 文档测试 | 随 `cargo test` 一并跑 | doc-tests | 无 |

一次跑全部:`cargo test`(单进程内全绿,无需任何外部依赖)。

### e2e 子模块(`tests/e2e/main.rs` 聚合)

- `e2e_tests` / `spot_e2e_java_parity_tests` — 现货端到端 + Java 对拍
- `futures_e2e_tests` — 合约端到端
- `loan_e2e_tests` — 现货借贷端到端
- `liquidation_e2e_tests` — 强平 / ADL / IF(含 proptest,回归种子在 `tests/proptest-regressions/`)
- `parallel_determinism` — 并行批量扫描的确定性(含 proptest)
- `full_snapshot_roundtrip` — 快照存取 roundtrip
- `java_snapshot_dat_interop` — 加载 Java 单进程富快照 `.dat`,验证 Rust Chronicle 读取器正确性(读入库 fixture,离线自足)
- `live_cluster_snapshot_interop` — 加载真实 raft 集群多分片生产快照(读入库 fixture)

## Fixture

全部入库在 `tests/`,测试默认离线可跑:

| 目录/文件 | 用途 | 来源 |
|---|---|---|
| `tests/conformance_vectors/*.{stream,golden}` | 一致性对拍向量 | Java `ConformanceExporter` 导出;新增见「脚本」 |
| `tests/snapshot_fixtures/rich_{re,me}0.dat` | 富状态单进程 Java 快照;被 `src` 序列化单测与 e2e `java_snapshot_dat_interop` 读取 | Java `SnapshotDatProduce`;刷新见「脚本」 |
| `tests/snapshot_fixtures/{re,me}0.ecs` | exchange-core `SnapshotProduce` 的极小 LZ4 `.ecs` 快照;被 `src` 序列化/帧解析单测读取 | Java `SnapshotProduce` |
| `tests/snapshot_fixtures/live_{re_0,re_1,me_0,me_1,me_2,me_3}.dat` | 真实 raft 集群多分片生产快照 | 见 route B 流程(起集群 → `POST /raft/snapshot` → 拷分片) |
| `tests/proptest-regressions/*.txt` | proptest 失败种子回归 | proptest 自动写入 |

## 脚本(`scripts/`)

可从任意目录运行;内部自行 `cd` 到 crate 根。

### `scripts/gen_snapshot_dat_fixtures.sh`

刷新入库的 `tests/snapshot_fixtures/rich_{re,me}0.dat`。仅当 Java 快照格式或 `SnapshotDatProduce` 场景变化时才需重跑。
流程:Java 生成器落盘 `/tmp` → 覆盖入库 fixture → 跑 `java_snapshot_dat_interop` + `--lib snapshot` 验证。

```bash
./scripts/gen_snapshot_dat_fixtures.sh
# raft-exchange 不在 crate 上级目录时:
RAFT_EXCHANGE_DIR=/path/to/raft-exchange ./scripts/gen_snapshot_dat_fixtures.sh
```

改动后记得提交 `tests/snapshot_fixtures/rich_{re,me}0.dat`。

### `scripts/conformance_live_diff.sh`

Java↔Rust「live 差分」:每次生成新鲜随机命令流,走 gen → Java 导出 golden → Rust replay 对拍,覆盖远超入库的固定向量。

```bash
./scripts/conformance_live_diff.sh [seed_base]   # 省略则用当前 epoch 秒
```

两个脚本都需要本机能跑 `cargo` 与 `mvn`,且能访问 raft-exchange(含 Java exchange-core / server 模块)。

## 基准测试(`benches/`)

```bash
cargo bench --bench engine_throughput      # 纯引擎吞吐:绕开 Raft 直灌 process_command
cargo bench --bench parallel_scan_bench    # 单命令停顿:funding/强平在 N 持仓下阻塞单管线的墙钟
```
