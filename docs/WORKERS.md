# 本机 CPU sidecar

两个独立 C++ 进程分别链接锁定的 whisper.cpp 和 llama.cpp 静态 C API 库，各自链接对应 ggml；没有链接 llama-common、CLI、cpp-httplib 或 HTTP 客户端。前端和 Rust 主进程不加载这些引擎动态库。

当前支持的是本机实测 `x86_64-unknown-linux-gnu` CPU 构建。Windows Job Object、其他平台原生构建、GPU 加速和完整 UI 生命周期验收尚未完成；本文件不能作为 V07、V09 或 V11 通过证据。

## 构建

常规构建直接运行以下命令；缺少所选档源码或静态库时，脚本会调用锁定的引擎构建入口并生成新的证据文件，不覆盖旧记录。已有源码的提交/工作树检查仍保留。默认只构建 portable，不要求旧 CPU 生成 AVX2：

```bash
python3 scripts/build-workers.py --jobs 1
python3 workers/smoke.py --output docs/evidence/workers-next-smoke.json
```

脚本检查引擎提交与锁清单一致且源码无已跟踪文件修改，单次最多 2 个编译任务。`build/workers/voice-asr-worker` 与 `voice-llm-worker` 是直接测试入口；打包副本以原子替换方式写入 `src-tauri/binaries/<name>-x86_64-unknown-linux-gnu`，不会对正在使用的旧副本就地覆盖。应用打包仍需把 worker 和引擎清单一起纳入验证。

另有显式 AVX2/FMA/F16C/BMI2 优化档：对应脚本传入 `--profile avx2`，产物放在 `build/workers-avx2/`，命名为 `voice-asr-worker-avx2`、`voice-llm-worker-avx2`，打包副本保留 `-avx2-<target triple>`。它们与 portable 同时保留；supervisor 必须在运行时检查四项 CPU 特征且资源存在才选择优化档，不能用优化档覆盖旧 CPU 的基线。

## 帧格式

4 字节小端无符号长度，后接 1 字节类型及负载。长度包括类型字节，上限 4 MiB，下限 1。类型 1 为 UTF-8 JSON，类型 2 为单声道 16 kHz f32 小端 PCM。控制 JSON 最大 64 KiB；PCM 最大 960,000 样本，必须有限且归一化至 [-1, 1]。PCM 只在内存中驻留，消费、错误、取消或卸载后释放，不写 WAV 临时文件。

stdout 仅发送类型 1 帧，stderr 不打印输入、转写或模型输出。引擎日志回调显式静默。未知类型、无效长度、非法 JSON、未知操作/字段及非法 job ID 会发送脱敏 error 后退出 2。业务失败保留进程并发送 error；supervisor 仍须验证阶段及 job ID。

## 消息合同

启动时主动发送：

```json
{"event":"hello","protocol":1,"worker":"asr"}
```

LLM 的 worker 值为 `llm`。不需要再发送 hello 命令。请求使用 `op`，响应使用 `event`：

| 请求 | 响应 / 行为 |
|---|---|
| `{"op":"load","model_path":"绝对路径","threads":2,"context_tokens":4096,"gpu_layers":0}` | `{"event":"ready","backend":"cpu"}`；LLM 另带实际 context_tokens。threads=0 自动取至多 4 线程；显式值不得超过逻辑核数。当前 gpu_layers 仅接受 0，其他值报 gpu_not_available |
| ASR：单个类型 2 PCM 帧，随后 `{"op":"run","job_id":"...","language":"auto"}` | PCM 无单独 ack；成功返回 `{"event":"result","job_id":"...","text":"引擎原文"}`。language 允许 auto/zh/en，不做翻译，不修剪或改写引擎文本 |
| LLM：`{"op":"run","job_id":"...","messages":[{"role":"system","content":"..."},{"role":"user","content":"..."}],"max_tokens":768}` | 使用模型内置 chat template，按 greedy/temperature 0 生成。成功 result 的 text 是模型完整输出字符串，单对象 JSON 结构及保真由 Rust validator 再验证 |
| `{"op":"cancel","job_id":"..."}` | 设置活动或已排队任务的取消标志；完成时返回 cancelled。**收到 cancelled 后 supervisor 应丢弃该 worker，下一任务重建并重新 load** |
| `{"op":"unload"}` | 释放上下文/模型并返回 `{"event":"unloaded"}` |
| `{"op":"shutdown"}` | 返回 `{"event":"bye"}` 并退出。supervisor 关闭 stdin 同样使进程立即退出 |

job_id 为非空、最大 128 字节的字符串。LLM messages 必须恰好是 system、user 两项，内容总大小不超过 32 KiB，禁止内嵌 NUL；max_tokens 范围 1–1024。上下文范围 2048–16384，且不得超过模型上限；实际 token 数加完整输出预算超限时在 decode 前返回 context_budget_exceeded。没有可识别 chat template 时明确失败，不猜测提示模板。

LLM 输出到 EOS 才返回 result；达到 token 上限仍未结束会返回 output_truncated。完整输出最大 128 KiB，不剥离代码围栏、不把结构不合格的回复当作成功整理结果。用户文本应由上游 PromptBuilder 放入 JSON 字段，并保留系统与用户消息边界；这里不负责替代语义保真检查。

本地输出另由固定 GBNF 限定为仅含 text 字符串的单对象 JSON，支持换行/引号/反斜杠的 JSON 转义和 Unicode。约束器在 greedy 选择前执行，EOS 也必须通过 grammar；每个 token 只 accept 一次。常见合法最高 logit 候选只检查该 token，被拒绝时才回退全词表 grammar mask + greedy，不使用无约束或 top-k 近似兜底。开发 reference 的四组同输入/完整输出 SHA 已一致；墙钟未稳定改善，不能单据此称性能达标。

resident LLM 只复用成功任务 prompt token 的公共前缀；所有 prefill 显式按 128 token 块处理，复用长度向下对齐 128，且严格小于新 prompt 长度。这样不复用旧 partial block，并至少重算末尾 prompt token。先移除 keep 之后的全部旧 prompt/生成 token，`llama_memory_seq_rm` 不支持时回退全清；任意 worker 错误、取消或卸载都会清除保存的 token 与 KV。

首次未对齐的实现曾在换 user 时把公开测试 `Hello 456.` 变成 `Hello.`，该失败和诊断完整保留，未放宽 SHA 或事实检查。改为双方一致的 128 块后，first、同 user 暖、换 user、换 system、error 后、cancel 重建后六例均与全清 reference 的完整输出 SHA 一致：实际留存 0/128/128/0/0/0。同 user 例全清 8.327 秒、缓存 5.272 秒，换 user 例 5.147/2.138 秒；只是开发 A/B，不是 V11 P95。证据为 `workers-kv-prefix-ab-2026-11-06-r2.json`。测试用 cache-probe/reference 可返回留存计数，二者不复制进包或开放给产品 UI。

load 阶段会拒绝非 multilingual Whisper，并用固定合成 system/user 消息检验 LLM 模板可渲染后才发 ready；此步骤不运行额外生成、不发送用户文本。run 时的模板验证继续保留。

错误形式：

```json
{"event":"error","job_id":"任务编号（若有）","code":"脱敏错误码","retryable":false}
```

包括 model_load_failed、model_not_loaded、invalid_pcm、asr_failed、context_budget_exceeded、output_truncated 等；错误不回显模型路径、输入或回复。无任务的 load/协议错误不带 job_id。

## 取消和父进程退出

stdin 独立 reader 始终读取控制帧；计算线程不会阻止取消请求或 EOF。EOF、截断输入流或父进程断开时直接退出整个进程，终止该进程的全部引擎线程。没有额外推理子进程或监听 socket。

真实 smoke 发现固定 Whisper 1.9.5 的 abort_callback 在完整 encoder graph 后才被检查，portable Base 下可延迟数十秒，首轮 smoke 因此失败。这不是取消已完成的证据。保留原始失败记录，修复方式是先请求协作取消，在 350 ms 宽限后如任务仍活动则发送一次 cancelled 并立即退出进程；不修改固定引擎源码。正常协作取消和 watchdog 使用同一互斥状态，避免双发 cancelled。supervisor 还须执行规格要求的 500 ms 上限与进程回收，不依赖引擎 callback 必定及时响应。

## 验证边界

`workers/smoke.py` 用真实模型和授权的 JFK fixture 验证协议、畸形帧、CPU 加载/转写/JSON 生成、预算和截断、计算中取消、重新建进程、unload/shutdown 及计算中 stdin EOF。它记录内容哈希和脱敏结果，且证据文件拒绝覆盖。

原始 `workers-smoke-2026-11-06.json` 记录了 ASR 取消超时；修复后的新文件独立保留，不能覆盖该失败。所有计时均是开发 smoke，可能与主工程编译并行，不是 V11 的冷/暖性能验收。UI 响应性、真实麦克风拔插、挂起恢复、打包进程生命周期以及跨平台仍需各自实测。

portable 的 r2 记录了取消修复成功和 Qwen 正确 JSON 生成，但后续 token 截断用例在 scalar prefill 阶段超过 30 秒，因此整轮未通过。AVX2 优化档已用同样的严格断言和时限重跑，没有提高超时门槛。

2026-11-06 优化档完整 smoke 已实际通过 62 项：同一 JFK 原文哈希与 portable 一致，ASR 4.204 秒、短 JSON 生成 3.019 秒；取消回执 ASR 0.3504 秒、LLM 0.0049 秒。预算拒绝、截断拒绝、取消后重建、unload/shutdown、计算中 EOF、畸形帧及零 stderr 均通过，详见 `docs/evidence/workers-smoke-avx2-2026-11-06.json`。这些单次数据不是 P95 或完整 UI 验收。

最终 portable wrapper 的只读协议检查另通过 28 项，未把它声称为 scalar 全套模型测试通过。四个可执行文件与 Tauri 打包副本 SHA-256 一致，两个实际模型完整 SHA-256 再次匹配锁清单；模型、fixture、源码、二进制及系统信息见 `docs/evidence/workers-build-provenance-2026-11-06.json`。`readelf`/`nm` 审计没有发现引擎动态库或直接网络函数导入；这不能代替完整应用的 V03 运行期网络审计。

最终冻结记录为 `docs/evidence/workers-build-provenance-2026-11-06-final.json`：9 个实际源码/锁清单指纹与当前文件一致，portable/AVX2 共四个 build 产物与四个 Tauri 资源副本 SHA 全部一致，两模型完整 SHA 再次匹配。最终代码包含固定 grammar、greedy 快路、128 对齐 KV 和两项 load 守卫；`workers-smoke-avx2-2026-11-06-final.json` 的 66 项严格检查通过，最终 portable framing 的 28 项见 `workers-protocol-portable-final-r2.json`。更早的 62/66 项快照与失败记录保留作阶段证据，不把它们全部当作最终版本的同一轮验收。
