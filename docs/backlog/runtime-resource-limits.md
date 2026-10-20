# `<runtime>` 层写出实际可用的 CPU 与内存

- 状态：idea
- 涉及 ADR：0036（0a `<runtime>` 层）
- 来源：Terminal-Bench smoke（2026-10-20）

## 背景（已核实）

- `compile-compcert`（任务 `cpus = 2`）：模型按宿主看到的 16 核跑了 `make -j16`，约 10 分钟没有进展，查内存后才改 `-j3`；最后在第 37 分钟（上限 40 分钟）才开始编 CompCert 本体，得 0 分。
- `<runtime>` 层今天写工具清单、档位与沙箱事实（ADR 0036 0a），不写 CPU/内存。

## 待决方向

- 宿主启动时读一次有效限额（Linux：cgroup v2 `cpu.max`、`memory.max`，否则 `available_parallelism`；macOS：`available_parallelism` 与物理内存），在 `<runtime>` 层写一行「可用 CPU N、内存 M」。会话内不变（与 0a 的不变量一致）。
- 对 CI、容器、herdr worker 都有用，不只为评测。

## 验收

cgroup 限 2 核的容器里 `<runtime>` 写 2 而不是宿主核数；`compile-compcert` 不再出现 `-j` 大于可用核数。
