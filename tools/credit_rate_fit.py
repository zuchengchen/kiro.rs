#!/usr/bin/env python3
"""Fit Kiro credit rates from traces.db and backtest credit_cache_reconcile (read-only).

Usage: tools/credit_rate_fit.py /home/czc/kiro-rs/data/traces.db [model] [since]
"""
import sqlite3
import sys

db = sys.argv[1]
model = sys.argv[2] if len(sys.argv) > 2 else "claude-opus-5.5"
since = sys.argv[3] if len(sys.argv) > 3 else "2026-09-23"
MIN_CC, MIN_GAP, HIT_T, MAX_T = 20_000, 0.5, 0.75, 1.25  # keep in sync with credit_cache_reconcile.rs
# Constants currently shipped in credit_cache_reconcile.rs (credits per 1M tokens).
SHIPPED = (3.370, 12.069, 527.0)

conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
rows = conn.execute(
    "select trace_id, input_tokens, cache_creation_tokens, cache_read_tokens, output_tokens, credits from traces "
    "where ts >= ? and final_status = 'success' and credits > 0 and usage_source = 'simulated' and model = ?",
    (since, model),
).fetchall()
if len(rows) < 200:
    sys.exit(f"{model}: only {len(rows)} samples, not enough to fit")


def solve3(a, b):
    m = [a[i][:] + [b[i]] for i in range(3)]
    for k in range(3):
        p = max(range(k, 3), key=lambda r: abs(m[r][k]))
        m[k], m[p] = m[p], m[k]
        for r in range(3):
            if r != k:
                f = m[r][k] / m[k][k]
                m[r] = [x - f * y for x, y in zip(m[r], m[k])]
    return [m[i][3] / m[i][i] for i in range(3)]


def rel_err(coef, x, y):
    return abs(sum(k * v for k, v in zip(coef, x)) - y) / y


data = [((cr / 1e6, (i + cc) / 1e6, o / 1e6), y) for _, i, cc, cr, o, y in rows]
keep = data
for _ in range(6):  # trimmed least squares: refit on the best 80%
    a = [[sum(x[p] * x[q] for x, _ in keep) for q in range(3)] for p in range(3)]
    coef = solve3(a, [sum(x[p] * y for x, y in keep) for p in range(3)])
    cut = sorted(rel_err(coef, x, y) for x, y in data)[int(len(data) * 0.8)]
    keep = [(x, y) for x, y in data if rel_err(coef, x, y) < cut]
read, uncached, output = coef
median = sorted(rel_err(coef, x, y) for x, y in data)[len(data) // 2]
print(
    f"{model}: n={len(data)} read={read:.3f} uncached={uncached:.3f} output={output:.3f} credits/1M "
    f"(median rel err {median:.1%})"
)
drift = max(abs(f - s) / s for f, s in zip(coef, SHIPPED))
print(f"max drift vs shipped constants {SHIPPED}: {drift:.1%}" + ("  <-- update constants" if drift > 0.10 else ""))

# Backtest with the shipped constants, i.e. what the running binary would do.
s_read, s_uncached, s_output = SHIPPED
hits = []
for trace_id, i, cc, cr, o, y in rows:
    if cc < MIN_CC:
        continue
    as_miss = (s_uncached * (i + cc) + s_read * cr + s_output * o) / 1e6
    gap = (s_uncached - s_read) * cc / 1e6
    if gap <= 0 or gap < MIN_GAP * as_miss:
        continue
    t = (as_miss - y) / gap
    if HIT_T <= t <= MAX_T:
        hits.append((trace_id, cc, t))
print(f"backtest: {len(hits)} rows would be reconciled, {sum(cc for _, cc, _ in hits)} creation tokens -> read")
for trace_id, cc, t in hits[-20:]:
    print(f"  {trace_id[:8]} creation={cc} t={t:.2f}")
