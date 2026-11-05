# V1 验收记录

依据 [规格第 10 节](../specs/v1.md)。更新日期：2026-11-06（Asia/Shanghai）。**当前整项完成 1/11（V05 A），尚不能作为 V1 发布通过。** 已有 Linux 开发实现、真实模型/采集/界面组件证据和自动测试；不能继续沿用最初“桌面 socket 全部拒绝、应用未实现”的状态描述。

本记录按已完整结束的证据快照评估：workspace 测试/Clippy **final**、优化 worker smoke **final（66 项）**、桌面探针 **r4**。后续源码和二进制仍在变化，旧日志不自动证明新版本已通过。历史失败日志保留；例如 workspace r6 的 Tauri 生命周期编译错误已由后续 r7 / final 重跑通过，不能隐藏 r6 或把它当成当前仍未修复。

## 参考机、模型与证据口径

- Intel Core i7-4770HQ，4 核 8 线程，x86-64，约 16 GiB RAM；Omarchy 4.0.6、Hyprland 0.57.0、Linux 7.2.15-1-omarchy、glibc 2.44。精确机器/依赖信息见 [引擎与产物来源记录][provenance]及 [M0 桌面探针][desktop]。
- Whisper Base 多语：`60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe`。
- Qwen2.5-1.5B-Instruct GGUF Q4_K_M：`6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e`。固定 revision、完整文件大小/哈希/许可见 [模型清单](../models.lock.json)及 [本地校验记录](evidence/models-verification.json)。
- 当前实际推理为 CPU portable / 独立 AVX2 优化档。开发 smoke 区分过首次进程加载与驻留任务，但 OS 页缓存和并发开发负载未统一；不得据此认定 V11 warm P95。
- [workspace final 测试][tests]共记录 85 个 harness 测试通过（audio 2、core 22、platform 12、runtime 41、crash 2、service 6）、失败/忽略均为 0，其中包含一个只供崩溃子进程调用的 helper；[Clippy final][clippy]完成无错误。测试数不等于 V 项通过数。

**A 与 M 分开记：** 本地 HTTPS 测试使用真实 TLS 传输与模拟供应商响应；真实 SIGKILL 测试使用真实子进程/SQLite 与模拟 native adapter，均属于 A。实际公开 WAV 推理、短时麦克风采集、Tauri DOM 启动是组件实测，不能拼接成尚未执行的完整桌面流程。静态 ELF 无网络导入不能代替运行期网络审计，构建/归档检查也不能代替安装/卸载。

## 第 10 节逐项矩阵

| ID | 自动测试 A / 软件证据 | 真实系统或桌面 M 证据 | 整项状态与必要缺口 |
|---|---|---|---|
| **V01 M** | 非 A 验收项；[UI smoke][ui]验证了真实 Tauri/WebKit DOM、准备检查、设置导航和 CLI status，未录音或注入。 | [M0][desktop]默认来源内存采集 8000 样本；[cpal 探针][capture]实际采得 7801 个有限样本、无新音频文件。受控 Wayland/XWayland 粘贴均未取得焦点，未通过。 | **未完成。** 缺 20 次真实快捷键录音、长按无重复、原生 Wayland/浏览器/XWayland 三类输入目标的中英/多行/特殊字符逐字交付。物理键释放未验收，不能以 Lua helper 模拟或 CLI status 替代。 |
| **V02 A/M** | [A-final][tests]平台测试覆盖切焦后切回仍拒绝、关闭/地址复用、事件断开/重连、锁屏与解锁失效、终端/敏感应用手动、同目标仅一次 attempt。核心测试覆盖提交门槛和取消后交付 Unknown。 | [M0][desktop]实际收到 socket2 开窗/关窗/焦点事件；没有完成延迟处理期间切焦/关闭/锁屏的完整应用操作，也没有成功粘贴消费证据。 | **部分 A 通过，M 未完成。** 仍缺 2 秒延迟 provider 的完整守卫矩阵、wtype 缺失/拒绝/超时的结果可访问性集成测试，以及未知交付不重放的实际应用确认。自动交付未获实测确认时保持禁用/手动。 |
| **V03 A/M** | [来源审计][provenance]核对最终 worker/模型/源码身份及构建边界，生产 worker 清空环境；静态边界本身不证明运行期无网络。 | [隔离网络报告][offline]中 loopback 关闭、无外部接口/路由，真实生产 WorkerPool 完成 15 条语料运行，外部探测返回 ENETUNREACH；[正常联网 strace 审计][online]覆盖另一次真实公开 WAV 链路及所有子进程，IPv4/IPv6 网络调用为 0。[最终 JFK 链路][pipeline]本地 ASR/LLM/保存/恢复通过。 | **开发链路已验证，整项未完成。** 上述是内核断网及正常联网两种实际运行期证据，但均无 GUI 麦克风/粘贴，不覆盖 GUI 空闲后台。因此仍缺断网采集至粘贴、恢复网络后的完整应用复测和后台审计，不能把一次两 job 的 strace 扩大为所有使用场景保证。 |
| **V04 A/M** | [A-final][tests]真实本地 HTTPS fixture 验证基地址路径、Bearer/最小 payload、401/403/404/429/5xx、拒绝重定向/无效证书、读体总 deadline、取消、空/脏/截断/工具响应拒绝；实现显式禁自动重试。 | 尚无真实兼容服务的合成文本测试，也无中英文录音经远程模式完成应用全流程的证据。 | **部分 A 通过，M 未完成。** 仍缺断网等完整错误矩阵逐例请求计数、错误后 Raw 逐字与禁止自动交付的集成验证、无新采集的重试及重启恢复、真实兼容服务测试。模拟服务响应不能算供应商实测。 |
| **V05 A** | [A-final][tests]核心 22 项覆盖 Raw 先提交、配置快照、忙时切换、重复停止、晚到结果、取消、重试预览、8 KiB 输入/12 KiB 输出、非法控制字符、交付前持久化门槛、TTL/限量和删除。新 [真实进程故障测试][crash]在 Raw 提交后、durable attempt 后/模拟 native 前、模拟 native 后/结果提交前三阶段 SIGKILL，重开 Raw 保留、attempt 转 Unknown、Restore/Tick effects 空、计数不重放，final 已通过。 | 不适用；此项要求 A。真实 SIGKILL 不会把临时文件模拟的 native 副作用变为真实桌面验收。 | **通过 A。** [§4 状态覆盖映射](evidence/v05-state-coverage.md)逐行关联测试；final 已通过新增八个异步边界的直接 Failed、取消/Busy/晚到，以及录音 Toggle/重复 Stop。Raw/processed 保留、错误 job/generation、无工作副作用和原生派发后 Unknown 均有断言；真实 SIGKILL 补齐提交前后恢复证据。此通过仅是 V05 A，不代表真实音频句柄、桌面交付或 V07 生命周期验收。 |
| **V06 A/M** | [A-final][tests]模型管理测试（包括新增 6 项模型/硬件建议测试）覆盖坏 hash/长度、中断/取消、空间预算、格式和加载失败、原子发布、版本碰撞、索引损坏、运行引用/选用保护、崩溃 part 清理与旧文件保留；测试中的加载闭包多为模拟。 | 两个官方模型已真实取得并完整核验；[JFK 链路][pipeline]用真实 ModelManager/worker 完成导入、加载。尚无设置页下载/中断/更新/回滚的完整人工记录。 | **部分通过。** 已实现只读架构/CPU/内存/可用磁盘探测，以及基于已验证模型清单、启动时可用内存和显式粗估预算的确定性建议；未知/不足资源不虚构本地支持，也不会自动切远程。相关 6 项测试验证内存解析、边界/未知资源、建议确定性、模型元信息/兼容旧索引及探测无副作用。GPU 明确未探测，当前仅 CPU；GPU 失败后显式 CPU 重跑仍无实现/验收证据。仍需真实模型生命周期和资源故障场景，不把开发下载脚本当作产品下载流程验收。 |
| **V07 M** | [A-final][tests]宿主真实 subprocess fixture 覆盖写管道堵塞、无取消回执、stderr 上限、错误 job ID、回收和模型引用；其中 fixture 不是实际推理引擎。 | [最终优化 worker 66 项][workers]真实模型通过计算中取消、取消后重建、unload/shutdown、计算中 stdin EOF、限定退出与零 stderr。只属 worker 组件实测。 | **未完成。** 缺实际 UI 推理中杀死两种 worker 后响应/重试、应用父进程异常退出后 5 秒内无推理孤儿、空闲期释放的完整应用实测，以及挂起/恢复、麦克风拔插和下一次录音。V05 的 helper SIGKILL 不能当作此项父/子推理进程验收。 |
| **V08 A/M** | [A-final][tests]已验证端点/档案凭据隔离、确认文件不含合成 Key、session secret 不持久化、关闭/清空/过期恢复记录为空、secure_delete/收缩后原文 marker 不在 DB、坏/未来配置不覆盖；宿主清空子进程环境。 | [采集探针][capture]未写音频文件，[最终 worker][workers]所测 stderr 为 0。没有完整生产应用的多载体 Key 检索或 PCM 落盘/出网审计。 | **部分 A 通过，M 未完成。** 仍需用合成 Key 检索实际日志/配置/DB/子进程命令行，核对 PCM 生命周期与网络；在实际 UI 确认换端点需重新确认/录入、持久化开关/清空/过期后列表为空。公开 fixture 的显式开发报告不是默认产品日志。 |
| **V09 M** | 跨平台源码或打包脚本不算 M；当前自动测试环境为 Linux。 | 仅本机 Linux 开发构建和隔离的 [Tauri UI smoke][ui]。没有 Windows 11、macOS Apple Silicon/Intel、Linux X11 独立桌面的发布包安装实测；Hyprland 的 XWayland 也未成功受控粘贴。 | **未完成。** 其他平台原生运行时/适配、安装包、授权拒绝/冲突/UIPI/剪贴板占用路径及其他 Wayland 手动流程仍需实现与实测。归档存在也不能记作安装/卸载通过。 |
| **V10 A/M** | 已冻结 [12 条不同句子真人录音 + 2 条明确拼接 + 1 条静音][corpus]；[素材核验][corpuscheck]通过身份/大小/16 kHz mono PCM/静音；该核验文件本身不含推理或人工评分，实际推理另见右栏。词法 validator 的数字/路径/专名/中英否定、JSON 限制在 [A-final][tests]有测试。 | [JFK][pipeline]只有一个公开英文 fixture 的两次本地开发链路，不是整套语料；[本地 corpus 报告](evidence/v10-corpus-local-offline.json)已完成 15 条：14 次 ASR/LLM、13 条结构与保护词校验通过、1 条保护词丢失被拒绝且保留 Raw、1 条静音跳过。真实远程模式与人工可用率仍无证据。 | **未完成。** 缺自然口头禅、口述路径和自然单人混输；拼接不能冒充自然混输。本地整套已运行，但结构通过不证明 ASR 或语义质量。仍需真实远程模式、人工核对事实及至少 10/12 评分；静音软件路径已跳过 ASR/LLM，尚不能代替运行期网络审计。当前没有人工分数。 |
| **V11 M** | 不适用；单测或结构检查不能证明时延门槛。 | [最终 JFK 开发链路][pipeline]有首次进程/驻留两次测时（14.221 / 9.891 秒），另有旧日志驻留 17.878 秒；[KV/grammar A/B][cache]只是少量固定输入的等价性比较。[最终 worker][workers]单次时长明确受开发负载影响。 | **未完成。** 无 20 次同组 10 秒真实录音、规范 warm 状态、快捷键指示/Raw 提交/实际粘贴各段 P95、冷加载与远程耗时。最新单次驻留低于 15 秒、旧链路和语料耗时提示性能波动，但均与正式测法不同，不能据此伪造达标或修改 15 秒门槛。 |

## 接续验收记录规则

新增记录须写明实际设备/系统、源码及二进制身份、模型完整 SHA-256、后端/线程、冷/暖状态、输入、步骤、结果和证据路径；A 的模拟 adapter 与 M 的真实桌面必须标明。只有该行全部条件满足才将整项改为通过；不得把 smoke、测试总数、脚本存在或一次正确输出扩大解释。后续发现失败应保留原日志并给独立修复后记录。

当前无需改动验收门槛。继续需要可正常解锁操作的真实桌面、尚缺的原生目标平台，以及经明确配置确认的真实远程兼容服务。本文不把这些前提缺失当作功能已通过。

[tests]: evidence/workspace-tests-final.log
[clippy]: evidence/workspace-clippy-final.log
[desktop]: evidence/m0-desktop-2026-11-06-r4.json
[capture]: evidence/m1-cpal-probe.log
[ui]: evidence/m1-tauri-ui-smoke.json
[pipeline]: evidence/m2-public-pipeline-final.log
[workers]: evidence/workers-smoke-avx2-2026-11-06-final.json
[provenance]: evidence/workers-build-provenance-2026-11-06-final.json
[cache]: evidence/workers-kv-prefix-ab-2026-11-06-r2.json
[crash]: ../crates/runtime/tests/recovery_crash.rs
[corpus]: ../tests/fixtures/v10/manifest.json
[corpuscheck]: ../tests/fixtures/v10/verification.json

[offline]: evidence/v10-offline-network.json
[online]: evidence/local-online-network-summary.json
