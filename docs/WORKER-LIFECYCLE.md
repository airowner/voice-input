# 生产 WorkerPool 生命周期开发验证

`crates/runtime/examples/worker_lifecycle.rs` 是显式开发程序，仅在 Linux 下读取公开 JFK WAV、锁定的两模型以及 `/proc`；不调用麦克风、UI、键盘、远程服务或系统密钥库。它使用真实生产 `WorkerPool` API，不改生产接口。

运行方式：

```sh
cargo run -p voice-runtime --example worker_lifecycle --locked -j 1 \
  > docs/evidence/worker-pool-lifecycle.json \
  2> docs/evidence/worker-pool-lifecycle.log
```

运行会完整重算两模型 SHA-256/大小并对比 `models.lock.json`，使用 `build/workers-avx2` 的最终 worker（线程 4），报告实际 worker 哈希。它不重新下载或改写模型，不扫描和终止其他用户服务。

检查边界：

1. 分别预加载 ASR/LLM，通过 `/proc/self/task/*/children` 找到唯一的直接子 worker，核对 executable、PPid、Linux starttime。计算请求在自有宿主线程进行，主线程轮询 CPU tick 增长后再次核对身份，只向这个自有进程发送 SIGKILL。生产 pool 必须返回错误、清失效缓存；下一次同 pool 请求必须创建不同身份的 worker 并产生可校验的真实结果。主线程心跳仅证明宿主线程仍可调度，不能替代真实 UI 响应测试。
2. 独立的自有 holder 子进程持有两个真实驻留 pool；根程序核对 holder 与其两个 worker 身份后，只杀 holder。worker 必须因 stdin EOF 在五秒内退出（Linux zombie 算已退出，不宣称已被系统 init 回收）。此检查是驻留状态父进程死亡；不扩大为计算中 GUI 崩溃证据。
3. 使用生产默认 LLM 60 秒、ASR 120 秒 idle 配置，实际等待，不修改时钟或缩短阈值。示例每 250ms 调用生产 `reap_idle`，核对阈值后五秒内 worker 已退出。它证明 pool 与真实 worker 的释放集成；GUI service 的周期调度仍需独立验证。

证据：`docs/evidence/worker-pool-lifecycle.json` 为完整成功报告，`worker-pool-lifecycle.log` 保留编译/运行阶段与异常。只有运行正常结束且 JSON `passed:true` 才算本开发检查通过。此检查不产生 V11 P95，不宣称 V07 M 整项通过；仍缺真实 UI、挂起恢复、设备拔插和完整应用生命周期操作。

## 本次实际结果

2026-11-06，Linux x86-64，完整报告 [worker-pool-lifecycle.json](evidence/worker-pool-lifecycle.json) 为 `passed:true`，[日志](evidence/worker-pool-lifecycle.log)正常结束；[定向 Clippy](evidence/worker-pool-lifecycle-clippy.log)含 `-D warnings` 通过。运行后二进制/示例源码/原始报告身份见 [provenance](evidence/worker-pool-lifecycle-provenance.json)。

| 检查 | 实测 |
|---|---|
| ASR 计算中 SIGKILL | 观察到 CPU 增长 6 ticks、主线程 4 次心跳；47ms 返回 `worker_closed`，6.021s 内重建并完成 JFK 转写 |
| LLM 计算中 SIGKILL | CPU 增长 2 ticks、主线程 1 次心跳；187ms 返回 `worker_closed`，10.539s 内重建并通过 JSON/保护词校验 |
| 自有 holder SIGKILL | 两真实驻留 worker 约 1ms 内退出（五秒上限） |
| 默认 idle | LLM 60.216s、ASR 120.233s 释放，均在默认阈值后五秒内且已退出 |

上述单次耗时只用于故障/释放的有界验证，OS 页缓存未控制，也不是正式性能基准或 P95。其他任务/系统负载仍可能变化。
