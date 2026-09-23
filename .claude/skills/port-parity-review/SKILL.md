---
name: port-parity-review
description: Review the Rust port (exchange-core-rs) against its Java oracle (raft-exchange) for parity gaps that per-command conformance CANNOT catch — dropped invariant-maintaining callbacks, ported-but-unwired methods, and Rust-only compensating band-aids. Use when reviewing ported risk/matching/liquidation/loan code, when a state/index invariant is in doubt, or when the user asks for a 移植对齐 / port-parity review.
---

## Why this skill exists

The `removePositionRecord` bug: Java bundles `settle + onPositionClosed(prune index) + remove` into one choke. The Rust port translated the OBSERVABLE parts (settle, remove — checked by conformance) but dropped the INVARIANT-maintaining callback (`onPositionClosed`), and papered over it with a Rust-only lazy `retain`. Per-command conformance passed because the index only affects LATER funding/liquidation scans. State (`symbol_to_users`) silently drifted from the Java oracle.

The lesson: conformance golden vectors verify per-command output; they do NOT guarantee that ported STATE invariants match. This review targets exactly that blind spot.

**Oracle location:** Java is at `../../binance/raft-exchange/exchange-core/src/main/java/exchange/core2/core` (adjust if moved). Rust is this repo. Treat Java as the source of truth.

## The three checks

Run all three. For each finding, capture: the Java site(s), the Rust site(s), the concrete divergence, and a one-line fix.

### Check 1 — Dropped invariant-maintaining callback

A Java method that maintains shared state used by a LATER scan (an index of who-holds-what, an exposure map, a running total) must be called at EVERY corresponding Rust site. These callbacks are part of the method's contract, not incidental.

1. Enumerate Java's state/index-maintaining callbacks. Start from names like `on*Opened`, `on*Closed`, `sync*`, `reconcile*`, `update*Index`, `*HoldingSymbol`, plus any method that writes a `*ToUsers` / `*Exposure` / `*Index` field.
2. For each, grep ALL Java call sites and ALL Rust production call sites (exclude tests):
   - `grep -rn "onPositionClosed(" <java>` vs `grep -rn "on_position_closed(" src/ | grep -v test`
3. Map Java sites → Rust sites. A path that closes/opens/mutates the entity in Rust but does NOT hit the callback is a gap. Different total strategy (e.g. Rust reconciles-per-command instead of incremental) is fine ONLY if you can prove every mutating path is covered — write down that proof.

### Check 2 — Ported-but-unwired method (dead-code red flag)

A method ported from Java with ZERO production callers is almost always a missing wiring, not intentional dead code.

1. List `pub`/`pub(crate)` fns in the ported engines (risk_engine, liquidation_engine, loan_liquidation_engine, …).
2. For each whose name mirrors a Java method, grep production call sites (exclude `#[cfg(test)]`, `mod tests`, `fn *test*`, asserts).
3. Zero production callers → RED FLAG. Either it should be wired at a real close/open/settle site, or deleted. Do not leave it.

### Check 3 — Rust-only compensating band-aid

A Rust mechanism with no Java counterpart usually compensates for a missing Java invariant — find what it hides.

1. Grep production for: `retain(`, `lazy`, `prune`, `fallback`, `rescan`, `reconcile`, `rebuild`, and full-collection scans (`.values().any`, `.iter().filter` over ALL users) that run on a hot/scan path.
2. For each, ask: does Java do this? Search the Java oracle for an equivalent.
3. No Java equivalent → the mechanism is compensating for something. Trace back to the invariant Java maintains actively (usually a callback from Check 1). Either the callback is missing (fix that, delete the band-aid) or the band-aid is a deliberate, documented divergence (write down why Java doesn't need it).

## Reporting

Output one table, most-severe first. Columns: `Check | Location (file:line) | Java oracle | Rust | Divergence | Fix`. If a check is clean, say so in one line (e.g. "Check 2: no unwired ported methods"). End with a one-line verdict: any state-invariant gaps, or clean.

Do NOT report per-command behavioral bugs (that is conformance's job) or style. Only the three parity classes above. Verify each finding against the Java source before reporting it — no speculation.
