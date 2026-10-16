# Build timings: where the Verify loop spends its compile time

Date: 2026-10-15 · Worktree `oc/build-timings` (branch from `origin/main`) · **measurement and a
report; no product change**. `Cargo.toml` is unchanged — see "Options" for what a change would
need.

## Question

The full AGENTS.md Verify takes about 6–7 minutes; `cargo test --workspace` alone is quoted at
280–400 s and "compiling and linking the test binaries is a large part". Workers rebuild after
small edits all day. This measures, in the **debug profile the Verify uses**, where that time
goes: the cold build, the incremental rebuild after a one-line edit in four named crates, how
many test binaries the workspace links, the linker and profile settings in force, and which
options are worth their risk.

## Machine, method, load

| item | value |
|---|---|
| machine | Apple M4, 10 cores, macOS 27.0.1 (Darwin 27.0.0) |
| rust | rustc / cargo **1.99.0**; no `rust-toolchain`, so **not** the 1.88 MSRV |
| profile | workspace default `dev` (`Finished … [unoptimized + debuginfo]`); **no `[profile]` table anywhere** |
| target dir | this worktree's `target/` only, **7.0 GiB** after the cold build (deps 5.2 GiB); no other tree was cleaned |
| on-disk link output | 162 executables, **2.36 GiB** total (median 13.6 MB, max 88 MB) |

**The machine is shared and heavily loaded.** Every number carries its `uptime` load; 1-min
loads seen were 55–128. Absolute values are load-sensitive; counts, shares and structure are the
durable result.

Method:

- Cold: `cargo test --workspace --no-run --timings` after cleaning this worktree's `target/`
  only. `--timings` HTML is parsed for the per-unit table (`target/cargo-timings/cargo-timing.html`,
  the `UNIT_DATA` array). Wall from `/usr/bin/time -p`; CPU from its `user`/`sys`.
- Incremental: insert `let _ = 0u8;` in a private function (no semantic effect), then
  `cargo test --workspace --no-run`. The file is restored with **`cp -p`** (original mtime) and
  the next no-op rebuild must recompile **0** crates — it does, which proves the run measured
  only its own edit. (A first pass used plain `cp`, which bumps the mtime and made each run
  also rebuild the previous run's fan-out; those numbers are discarded.)
- Cargo's `--timings` `duration` is the wall of the rustc / build-script process. It does **not**
  separate the external linker.

## 1. Cold build (debug)

| metric | value |
|---|---|
| wall (`real`) | **110.9 s** (cargo: `Finished … in 1m 50s`) |
| CPU (`user` + `sys`) | 436.2 + 70.7 = **506.9 s** |
| mean parallelism | 506.9 / 110.9 ≈ **4.6×** on 10 cores (during a build the 1-min load was 107 → 128) |
| executables | **162** = 144 integration-test + 15 lib-test + 3 bin-test |
| size of those 162 | **2.36 GiB** (mean 14.9 MB, median 13.6 MB) |

Cargo attributes **1021.5 s** of per-unit duration (about 2× the real CPU, because under load 107
each unit's own duration absorbs waiting time; use the values as **shares**, not as CPU):

| category | Σ duration | n | avg |
|---|---|---|---|
| lib / rmeta (all dependencies + workspace libs) | 459.4 s | 305 | 1.51 s |
| **integration-test binaries** | **344.4 s** | **144** | 2.39 s |
| lib-test binaries | 88.5 s | 15 | 5.90 s |
| build-script runs | 85.2 s | 42 | 2.03 s |
| product binaries (`radian`, `rad`, …) | 22.5 s | 4 | 5.63 s |
| bin-test binaries | 8.1 s | 3 | 2.68 s |
| other | 13.4 s | 44 | 0.30 s |

**Test-binary work is 441 s of 1021 s — 43% of all attributed time**, and the largest category.
Workspace crates own 582 s of the 1021 s; dependencies (registry crates) own 439 s.

### Slowest units

| unit | duration |
|---|---|
| `radian-tui` build script (**run**) | **40.9 s** |
| `aws-lc-sys` build script (C) | 29.6 s |
| `radian-host` lib-test binary | 25.7 s |
| `jsonschema` lib (frontend 13.2 + codegen 12.2) | 25.3 s |
| `radian-host` lib | 21.9 s |
| `radian-tui` lib-test binary | 19.1 s |
| `rmcp` lib | 14.6 s |
| `agent-cli` `tests/cli.rs` | 14.2 s |
| `regex-automata` lib | 13.2 s |
| `syn` lib ×2 | 12.7 / 12.1 s |
| `radian-tui` `rad` bin | 12.3 s |
| `agent-extensions` `tests/mcp.rs` | 11.8 s |

### Critical path vs throughput

The longest dependency chain in the unit graph is **67.5 s**:

```
syntect (3.9 s) → radian-tui build script (0.5 s compile + 40.9 s run)
  → radian-tui lib (9.9 s) → radian-tui `rad` bin (12.3 s)
```

The wall (**110.9 s**) is far above that chain, so the cold build is **throughput-bound, not
latency-bound**: the aggregate work of 162 test links and 305 library units, not one chain, sets
the wall. Two consequences: cutting the *number* of test binaries helps even though no single
test link is critical; and the 40.9 s build script is on the critical path but overlaps other
work, so its cost does not add 40.9 s to the wall.

## 2. Incremental rebuild after a one-line change

`cargo test --workspace --no-run`, one `let _ = 0u8;` inside a private function:

| change | wall | load (1-min) | workspace crates recompiled |
|---|---|---|---|
| `agent-core/src/compaction.rs` | **22.8 s** | 55 | **all 12** |
| `radian-host/src/auth.rs` | **18.5 s** | 86 | `radian-host`, `agent-cli`, `radian-tui` |
| `radian-coding/src/args.rs` | **24.3 s** | 102 → 111 | `radian-coding`, `radian-host`, `agent-cli`, `radian-tui` |
| `radian-tui/src/app.rs` | **16.7 s** | 111 | `radian-tui` only |

No-op rebuild (nothing changed): **0.76 s**. Dependency fan-out (from each crate's `Cargo.toml`):

- `agent-core` — every crate depends on it → whole workspace (**worst case**, 22.8 s at load 55).
- `radian-coding` — depended on by `radian-host`, `agent-cli`, `radian-tui` (24.3 s at load 102–111).
- `radian-host` — depended on by `agent-cli`, `radian-tui` (18.5 s at load 86).
- `radian-tui` — **leaf**; only itself and its **40** test units (16.7 s at load 111).

The leaf edit is barely cheaper than the whole-workspace edit (16.7 vs 22.8 s): the cost is not
which crates recompile but **relinking the test binaries**. `radian-tui` alone has 40 of the 162.
A useful rule: **an edit costs roughly one test-binary relink per test file in the fan-out,
regardless of how much code changed.**

## 3. Test binaries

- **162 executables** link in a `cargo test --workspace --no-run`: **144 integration-test files**
  (`tests/*.rs` — one Cargo target and one link each), 15 lib-test binaries, 3 bin-test binaries.
  Plus the 2 product bins (`radian`, `rad`) and `sandbox-escape` with `testing`.

| crate | integration-test binaries | Σ all its test units |
|---|---|---|
| `radian-tui` | 37 | 106.9 s |
| `radian-host` | 19 | 63.2 s |
| `radian-session` | 18 | 33.3 s |
| `agent-core` | 16 | 52.6 s |
| `radian-runtime` | 13 | 59.7 s |
| `radian-coding` | 12 | 34.4 s |
| `radian-memory` | 9 | 6.8 s |
| `radian-sandbox` | 6 | 3.9 s |
| `agent-cli` | 5 | 28.2 s |
| `agent-provider-anthropic` | 3 | 15.3 s |
| `agent-provider-openai-compatible` | 3 | 13.3 s |
| `radian-permissions` | 2 | 3.8 s |
| `agent-extensions` | 1 | 14.7 s |

(Σ covers integration + lib-test + bin-test units, so it is slightly larger than the integration
count alone; 144 integration + 15 lib-test + 3 bin-test = 162.)

- **Biggest by build time**: `radian-host` lib-test 25.7 s, `radian-tui` lib-test 19.1 s,
  `agent-cli/tests/cli.rs` 14.2 s, `agent-extensions/tests/mcp.rs` 11.8 s.
- **Biggest on disk**: `radian` 88 MB, `radian-host/tests/session.rs` 62 MB,
  `radian-host/tests/access_switch.rs` 62 MB, `radian-runtime/tests/e2e_resume_compaction.rs` 42 MB.

## 4. Linker and profile settings (checked, not assumed)

- **Linker driver**: `cc` = Apple clang 21.0.0 (`arm64-apple-darwin27.0.0`), which invokes
  `/usr/bin/ld` (a 200 KB universal trampoline) → Xcode's `ld` (2.7 MB). `ld -v` reports
  `@(#)PROGRAM:ld PROJECT:ld-27037.1` — identified as the toolchain's **newer linker (`ld` /
  ld-prime)** by the version string, since the classic linker prints `PROJECT:ld64-<version>`.
  `lld`, `mold` are **not installed**.
- **Profile**: no `[profile]` table, so Cargo's defaults: `opt-level = 0`, `codegen-units = 256`,
  `incremental = true`, `panic = unwind`. The rustc command line confirms
  **`-C debuginfo=2`** (full) and **`-C split-debuginfo=unpacked`**.
- **What `unpacked` means here, observed**: the DWARF is written into the per-CGU **`.o` files**
  in `target/debug/deps` (that is the 5.2 GiB of the 7.0 GiB target). The final executables have
  **no `__DWARF` segment** and there is **no `.dSYM`** anywhere under `target/`; `dsymutil` does
  not appear in the build. So `split-debuginfo` is already at its macOS default and is **not** a
  lever.
- **Why the executables are 2.36 GiB anyway**: their size is the symbol table, not DWARF. For
  `radian-host/tests/session.rs` (62 MB): `__TEXT` 27.7 MB, **`__LINKEDIT` 36.4 MB** (113 910
  symbols), `__DWARF` 0.

### Direct measurement of the `radian-tui` build script

The build script binary was run three times in isolation (temp `OUT_DIR`, load ~104):

| run | wall |
|---|---|
| 1 | 11.1 s |
| 2 | 16.7 s |
| 3 | 27.4 s |

load-dependent 11–27 s single-threaded, versus 40.9 s attributed in the cold build (which ran it
alongside the whole build). This is the work `build.rs` does: load `syntect`'s default syntax
set, parse the vendored TypeScript/TSX/TOML YAML (~200 KB each) with `yaml-load`, and dump the
combined set — all in the unoptimized build-script profile.

## 5. Options, ranked

"Measured" is a number on this machine; "estimate" is a projection from the measured structure.
**No `Cargo.toml` change was made**, because no gain was measured end-to-end and the full Verify
was not run on a candidate.

### Option A — `[profile.dev.build-override] opt-level = 1..3` (lowest risk)

- **Measured**: the `radian-tui` build script is the **single slowest unit (40.9 s)**, head of the
  67.5 s critical path, and runs unoptimized; in isolation it takes 11–27 s. `aws-lc-sys`'s C
  build is second (29.6 s).
- **Estimate**: an optimized build-script run often speeds up 2–5×; 40.9 s → roughly 10–20 s.
  Cost: build-script / proc-macro *crates* recompile once, slower. **Cold-build win only** —
  build scripts do not rerun on a source edit, so this does **not** help the all-day loop.
- **Risk**: low; no product code. Needs measurement + full Verify.

### Option B — Fewer integration-test binaries (largest structural lever)

- **Measured**: **144** integration-test binaries, one link each, **344 s** of the 1021 s
  attributed total (43% with lib/bin tests); `radian-tui` has 37. The loop is link-bound
  (leaf edit 16.7 s for one crate), so removing links removes wall.
- **Estimate**: folding each crate's `tests/*.rs` into one `tests/it/main.rs` (`mod …;`) removes
  ~130 of the 162 links; a rough **15–25 s off the cold wall** and a proportional cut of every
  fan-out rebuild — the biggest effect on the all-day loop.
- **Risk**: **high**. Tests would share one process (ports, ptys, global state, crash/job
  injection, `#[test]` parallelism); the documented commands `cargo test -p agent-provider-*
  --features testing --test contract` (AGENTS.md) name single test files and would break; the
  `journal-selfcheck` runs replay every session in the merged suites. This is ADR-worthy, not a
  config tweak.

### Option C — `[profile.dev] debug = "line-tables-only"` (or `debug = 1`)

- **Measured**: `debuginfo=2` is on; the DWARF lives in the `.o` files (5.2 GiB), **not** in the
  executables (whose size is the symbol table). So this shrinks codegen's debuginfo work and the
  `.o` files, but **not** the executables or their link.
- **Estimate**: the win is on the compile side, so smaller than the usual 5–15%; the link-side
  saving is likely modest. **Not measured.**
- **Risk**: low (backtraces keep file:line; local-variable values are lost). Needs full Verify.

### Option D — `[profile.dev] strip = "symbols"` (addresses the real binary size)

- **Measured**: `__LINKEDIT` is ~59% of the `session` test binary (36.4 / 62 MB) with 113 910
  symbols; DWARF is not in the binaries at all.
- **Estimate**: stripping symbols would shrink each executable and cut link output, but loses
  symbolication (worse than line-tables-only). **Not measured.**
- **Risk**: medium (debugging backtraces lose function names).

### Option E — A faster linker

- **Measured**: the default is Apple's new `ld`; `lld`/`mold` are absent.
- **Estimate**: `lld` is often 1.5–3× faster on large links; a few seconds per fan-out. Requires
  installing LLVM and `-fuse-ld=lld`. `mold` does not target macOS. **Not measured.**

### Option F — Dependency trimming

- **Measured**: `aws-lc-sys`'s C build is 29.6 s (via `rustls` → `reqwest`); `libsqlite3-sys`
  8.9 s; `syntect` is heavy in `radian-tui` (build script above).
- **Estimate**: switching `rustls`'s provider to `ring`, or shipping a prebuilt syntax dump, could
  remove tens of seconds of cold work. **Risk**: TLS behaviour and provider-contract coverage.
  **Not measured.**

### Not levers (ruled out by measurement)

- `dsymutil` / `.dSYM` cost — none exists.
- `split-debuginfo` — already `unpacked` (the macOS default).
- Cargo change detection — a no-op rebuild is 0.76 s.

## 6. What this did not measure

- No option was taken end-to-end (no gain measured, no full Verify run on a candidate).
- Per-link time is inferred from unit durations (integration unit = small leaf compile + one
  link); `--timings` does not split the linker.
- Absolute numbers are for a shared machine at load 55–128; only one sample per incremental.
- Release builds, Linux, other linkers, and the effect of any Cargo.toml change are untested.

## Reproduce

```sh
# cold (this worktree only)
cargo clean
/usr/bin/time -p cargo test --workspace --no-run --timings
python3 - <<'PY'  # parse target/cargo-timings/cargo-timing.html's UNIT_DATA / CONCURRENCY_DATA
PY

# incremental: edit a private fn, then
/usr/bin/time -p cargo test --workspace --no-run
```

Raw timing data: `target/cargo-timings/cargo-timing.html` (regenerate with `--timings`).
