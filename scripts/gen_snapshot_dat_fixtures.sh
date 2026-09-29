#!/usr/bin/env bash
# 刷新入库的富状态 Java 快照 fixture(tests/snapshot_fixtures/rich_{re,me}0.dat)。
# 这两个 .dat 被 src 单测(snapshot::marshalling)与 e2e java_snapshot_dat_interop 用 include/读取,
# 平时无需重跑;仅当 Java 快照格式或 SnapshotDatProduce 场景变化时,用本脚本重新生成并覆盖入库。
#
# 生成器 = raft-exchange-server 的 SnapshotDatProduce(@Test,server MemorySerializationProcessor
# 不压缩 .dat,DUMP_ID=88888,单分片 Direct 订单簿),先落到 /tmp,再拷进 tests/snapshot_fixtures/。
#
# 用法: ./scripts/gen_snapshot_dat_fixtures.sh
# 前置: 已能 cargo / mvn。可从任意目录运行。
# 若 raft-exchange 不在 exchange-core-rs 的上级目录,用 RAFT_EXCHANGE_DIR 指定其根目录:
#   RAFT_EXCHANGE_DIR=/path/to/raft-exchange ./scripts/gen_snapshot_dat_fixtures.sh
set -euo pipefail

RS_DIR="$(cd "$(dirname "$0")/.." && pwd)"
RAFT_EXCHANGE_DIR="${RAFT_EXCHANGE_DIR:-$(cd "$RS_DIR/.." && pwd)}"
MVN="${MVN:-mvn}"
TMP_DIR="/tmp/rust_snapshot_dat_fixture"
DEST_DIR="$RS_DIR/tests/snapshot_fixtures"
GENERATOR="raft-exchange-server/src/test/java/com/binance/raftexchange/server/exchange/snapshot/SnapshotDatProduce.java"

if [[ ! -f "$RAFT_EXCHANGE_DIR/$GENERATOR" ]]; then
    echo "[fixtures] 找不到生成器: $RAFT_EXCHANGE_DIR/$GENERATOR" >&2
    echo "[fixtures] 用 RAFT_EXCHANGE_DIR=/path/to/raft-exchange 指定 raft-exchange 根目录" >&2
    exit 1
fi

echo "[1/4] Java 生成快照 → $TMP_DIR  (raft-exchange=$RAFT_EXCHANGE_DIR)"
(cd "$RAFT_EXCHANGE_DIR" && "$MVN" -q -pl raft-exchange-server -am \
    -Dtest=SnapshotDatProduce -DfailIfNoTests=false -Dsurefire.failIfNoSpecifiedTests=false test)

echo "[2/4] 校验产物"
for f in snapshot_88888_RE_0.dat snapshot_88888_ME_0.dat; do
    if [[ ! -s "$TMP_DIR/$f" ]]; then
        echo "[fixtures] 缺失或为空: $TMP_DIR/$f" >&2
        exit 1
    fi
done

echo "[3/4] 覆盖入库 fixture → $DEST_DIR"
cp "$TMP_DIR/snapshot_88888_RE_0.dat" "$DEST_DIR/rich_re0.dat"
cp "$TMP_DIR/snapshot_88888_ME_0.dat" "$DEST_DIR/rich_me0.dat"
echo "  rich_re0.dat ($(wc -c < "$DEST_DIR/rich_re0.dat" | tr -d ' ') bytes)"
echo "  rich_me0.dat ($(wc -c < "$DEST_DIR/rich_me0.dat" | tr -d ' ') bytes)"

echo "[4/4] 用刷新后的 fixture 跑相关测试"
(cd "$RS_DIR" && cargo test --test e2e java_snapshot_dat_interop && cargo test --lib snapshot)

echo "[fixtures] PASS ✓  (如有变化,记得提交 tests/snapshot_fixtures/rich_{re,me}0.dat)"
