# V1 验收记录

依据 [规格第 10 节](../specs/v1.md)。更新日期：2026-11-06（Asia/Shanghai）。**当前整项完成 1/11（V05 A），尚不能作为 V1 发布通过。** 已有 Linux 开发实现、真实模型/采集/界面组件证据和自动测试；不能继续沿用最初“桌面 socket 全部拒绝、应用未实现”的状态描述。

本记录按已完整结束的证据快照评估：解锁后 workspace **93 项测试及 Clippy**、优化 worker smoke **final（66 项）**、[原生输入汇总][native]及其逐项桌面证据。桌面已解锁，已取得生产适配的真实输入与回读；此前锁屏、wtype 不兼容和测试驱动失败记录继续保留。旧日志不自动证明变化后的源码或新 release/安装包已通过；本轮新生产构建及release启动已通过，安装包仍按独立记录验证。

## 参考机、模型与证据口径

- Intel Core i7-4770HQ，4 核 8 线程，x86-64，约 16 GiB RAM；Omarchy 4.0.6、Hyprland 0.57.0、Linux 7.2.15-1-omarchy、glibc 2.44。精确机器/依赖信息见 [引擎与产物来源记录][provenance]及 [M0 桌面探针][desktop]。
- Whisper Base 多语：`60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe`。
- Qwen2.5-1.5B-Instruct GGUF Q4_K_M：`6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e`。固定 revision、完整文件大小/哈希/许可见 [模型清单](../models.lock.json)及 [本地校验记录](evidence/models-verification.json)。
- 当前实际推理为 CPU portable / 独立 AVX2 优化档。开发 smoke 区分过首次进程加载与驻留任务，但 OS 页缓存和并发开发负载未统一；不得据此认定 V11 warm P95。
- [解锁后 workspace 测试][tests]共记录 93 个 harness 测试通过（audio 2、core 22、platform 20、runtime 41、crash 2、service 6）、失败/忽略均为 0，其中包含一个只供崩溃子进程调用的 helper；[解锁后 Clippy][clippy]完成无错误。新增协议/截止时间/进程组清理测试见 [原生键盘验证][keyboard]。测试数不等于 V 项通过数。

**A 与 M 分开记：** 本地 HTTPS 测试使用真实 TLS 传输与模拟供应商响应；真实 SIGKILL 测试使用真实子进程/SQLite 与模拟 native adapter，均属于 A。实际公开 WAV 推理、短时麦克风采集、Tauri DOM 启动是组件实测，不能拼接成尚未执行的完整桌面流程。静态 ELF 无网络导入不能代替运行期网络审计，构建/归档检查也不能代替安装/卸载。

## 第 10 节逐项矩阵

| ID | 自动测试 A / 软件证据 | 真实系统或桌面 M 证据 | 整项状态与必要缺口 |
|---|---|---|---|
| **V01 M** | 非 A 验收项；[UI smoke][ui]有真实 DOM/IPC 检查。[应用专用框][ownpaste]通过现有 typed UI 流程取得可信 paste/input、固定文本逐字回读，且只有确认后本次会话自动交付才启用。 | [M0 解锁复测][desktop]的采集/事件/owner通过；[cpal][capture]有真实短时采集。[生产适配的 Chromium Wayland][browserwl]与[XWayland][browserx]均有可信 paste/input 和逐字一致结果，使用隔离自有 profile；[20 次浏览器粘贴][paste20]全部通过。原 wtype 在 XWayland/WebKit 的失败另保留。 | **未完成。** 这些是固定合成文本的真实输入组件证据，未采集/推理，20 次粘贴不是 20 次录音。仍缺 20 次真人快捷键录音、物理长按无重复，以及录音完整链路在三类输入目标的逐字交付；[GTK原生](evidence/desktop-guard-native-wayland-r2.json)和[XWayland编辑器](evidence/desktop-guard-native-xwayland-r2.json)的生产adapter正向逐字回读及负例也已通过。虚拟键释放不能替代真人按键。 |
| **V02 A/M** | [A-final][tests]覆盖切焦后切回、关闭/地址复用、事件断连/重连、锁定/解锁、终端/敏感应用、一次 attempt、提交门槛和取消后 Unknown；新增原生协议测试覆盖缺失/畸形协议、最后焦点拒绝、派发后断连/缺 ACK 为 Unknown 且不重发，以及共享 deadline/持管道后代清理。 | [真实焦点负例][focusguard]在固定合成结果延迟至少 2 秒期间观察 A→B→A 的实际焦点 epoch 变化，原目标交付为 Manual；关闭目标也 Manual，自有输入框均未改变，合成结果仍可访问。该例未调用真实 provider 或完整应用控制器。 | **部分 A 与桌面组件通过，整项未完成。** 仍缺完整应用中的延迟处理/锁屏/IPC 断线/终端等矩阵，以及输入拒绝/超时后 Raw 可访问、未知交付不重放的实际集成确认。原规格 wtype 路径的实测兼容失败促成同协议内部 Ctrl+V 适配替换，未改规格或自行放宽验收；实际自动交付仍须每会话专用框确认及逐目标守卫。 |
| **V03 A/M** | [来源审计][provenance]核对最终 worker/模型/源码身份及构建边界，生产 worker 清空环境；静态边界本身不证明运行期无网络。 | [隔离网络报告][offline]中 loopback 关闭、无外部接口/路由，真实生产 WorkerPool 完成 15 条语料运行，外部探测返回 ENETUNREACH；[正常联网 strace 审计][online]覆盖另一次真实公开 WAV 链路及所有子进程，IPv4/IPv6 网络调用为 0。[最终 JFK 链路][pipeline]本地 ASR/LLM/保存/恢复通过。 | **开发链路已验证，整项未完成。** 上述是内核断网及正常联网两种实际运行期证据，但均无 GUI 麦克风/粘贴，不覆盖 GUI 空闲后台。因此仍缺断网采集至粘贴、恢复网络后的完整应用复测和后台审计，不能把一次两 job 的 strace 扩大为所有使用场景保证。 |
| **V04 A/M** | [A-final][tests]真实本地 HTTPS fixture 验证基地址路径、Bearer/最小 payload、401/403/404/429/5xx、拒绝重定向/无效证书、读体总 deadline、取消、空/脏/截断/工具响应拒绝；实现显式禁自动重试。 | 尚无真实兼容服务的合成文本测试，也无中英文录音经远程模式完成应用全流程的证据。 | **部分 A 通过，M 未完成。** 仍缺断网等完整错误矩阵逐例请求计数、错误后 Raw 逐字与禁止自动交付的集成验证、无新采集的重试及重启恢复、真实兼容服务测试。模拟服务响应不能算供应商实测。 |
| **V05 A** | [A-final][tests]核心 22 项覆盖 Raw 先提交、配置快照、忙时切换、重复停止、晚到结果、取消、重试预览、8 KiB 输入/12 KiB 输出、非法控制字符、交付前持久化门槛、TTL/限量和删除。新 [真实进程故障测试][crash]在 Raw 提交后、durable attempt 后/模拟 native 前、模拟 native 后/结果提交前三阶段 SIGKILL，重开 Raw 保留、attempt 转 Unknown、Restore/Tick effects 空、计数不重放，final 已通过。 | 不适用；此项要求 A。真实 SIGKILL 不会把临时文件模拟的 native 副作用变为真实桌面验收。 | **通过 A。** [§4 状态覆盖映射](evidence/v05-state-coverage.md)逐行关联测试；final 已通过新增八个异步边界的直接 Failed、取消/Busy/晚到，以及录音 Toggle/重复 Stop。Raw/processed 保留、错误 job/generation、无工作副作用和原生派发后 Unknown 均有断言；真实 SIGKILL 补齐提交前后恢复证据。此通过仅是 V05 A，不代表真实音频句柄、桌面交付或 V07 生命周期验收。 |
| **V06 A/M** | [A-final][tests]模型管理测试（包括新增 6 项模型/硬件建议测试）覆盖坏 hash/长度、中断/取消、空间预算、格式和加载失败、原子发布、版本碰撞、索引损坏、运行引用/选用保护、崩溃 part 清理与旧文件保留；测试中的加载闭包多为模拟。 | 两个官方模型已真实取得并完整核验；[JFK 链路][pipeline]用真实 ModelManager/worker 完成导入、加载。尚无设置页下载/中断/更新/回滚的完整人工记录。 | **部分通过。** 已实现只读架构/CPU/内存/可用磁盘探测，以及基于已验证模型清单、启动时可用内存和显式粗估预算的确定性建议；未知/不足资源不虚构本地支持，也不会自动切远程。相关 6 项测试验证内存解析、边界/未知资源、建议确定性、模型元信息/兼容旧索引及探测无副作用。GPU 明确未探测，当前仅 CPU；GPU 失败后显式 CPU 重跑仍无实现/验收证据。仍需真实模型生命周期和资源故障场景，不把开发下载脚本当作产品下载流程验收。 |
| **V07 M** | [A-final][tests]宿主 fixture 覆盖堵塞管道、取消、stderr、错误 job ID、回收和模型引用；fixture 不等同实际引擎。 | [最终 worker 66 项][workers]有真实模型取消/重建/EOF/卸载。[生产 WorkerPool 生命周期][lifecycle]实际杀死计算中的两种引擎，47/187ms 返回错误，下一 run 均重建成功；自有驻留 holder 被 SIGKILL 后两 worker 约 1ms 退出；按真实默认阈值，LLM 60.216s、ASR 120.233s 释放。 | **组件已验证，整项未完成。** 上述主线程心跳不是 UI 响应；holder 驻留退出不代表 GUI 计算中崩溃，example 的 250ms reap 调度也不是完整应用 timer 验收。仍缺实际 UI 中强杀后的响应/重试、应用父进程退出及空闲释放、挂起/恢复、麦克风拔插和下一次录音的完整记录。 |
| **V08 A/M** | [A-final][tests]已验证端点/档案凭据隔离、确认文件不含合成 Key、session secret 不持久化、关闭/清空/过期恢复记录为空、secure_delete/收缩后原文 marker 不在 DB、坏/未来配置不覆盖；宿主清空子进程环境。 | [采集探针][capture]未写音频文件，[最终 worker][workers]所测 stderr 为 0。没有完整生产应用的多载体 Key 检索或 PCM 落盘/出网审计。 | **部分 A 通过，M 未完成。** 仍需用合成 Key 检索实际日志/配置/DB/子进程命令行，核对 PCM 生命周期与网络；在实际 UI 确认换端点需重新确认/录入、持久化开关/清空/过期后列表为空。公开 fixture 的显式开发报告不是默认产品日志。 |
| **V09 M** | 跨平台源码或打包脚本不算 M；当前自动测试环境为 Linux。 | 已有本机 Linux 开发构建和隔离 UI 启动；本轮在 Hyprland 中的 Chromium XWayland 目标已通过生产适配逐字粘贴，但这不是独立 Linux X11 会话。无 Windows 11、macOS Apple Silicon/Intel、Linux X11 原生发布包安装实测。 | **未完成。** 其他平台原生运行时/适配、安装包、授权拒绝/冲突/UIPI/剪贴板占用，以及其他 Wayland 手动流程仍需实现与实测。XWayland 成功、归档存在均不能代替这些目标系统或真实安装/卸载验收。 |
| **V10 A/M** | 已冻结 [12 条不同句子真人录音 + 2 条明确拼接 + 1 条静音][corpus]；[素材核验][corpuscheck]通过身份/大小/16 kHz mono PCM/静音；该核验文件本身不含推理或人工评分，实际推理另见右栏。词法 validator 的数字/路径/专名/中英否定、JSON 限制在 [A-final][tests]有测试。 | [JFK][pipeline]只有一个公开英文 fixture 的两次本地开发链路，不是整套语料；[本地 corpus 报告](evidence/v10-corpus-local-offline.json)已完成 15 条：14 次 ASR/LLM、13 条结构与保护词校验通过、1 条保护词丢失被拒绝且保留 Raw、1 条静音跳过。真实远程模式与人工可用率仍无证据。 | **未完成。** 缺自然口头禅、口述路径和自然单人混输；拼接不能冒充自然混输。本地整套已运行，但结构通过不证明 ASR 或语义质量。仍需真实远程模式、人工核对事实及至少 10/12 评分；静音软件路径已跳过 ASR/LLM，尚不能代替运行期网络审计。当前没有人工分数。 |
| **V11 M** | 不适用；单测或结构检查不能证明时延门槛。 | [JFK][pipeline]有首次进程/驻留两次文件测时（14.221 / 9.891 秒），旧驻留日志为 17.878 秒。[本轮 20 次 Wayland 浏览器粘贴][paste20]生产 adapter 调用 P95 为 **194.804ms**，每次均有可信事件及逐字回读；计时只含 copy、焦点守卫与原生协议 ACK，键图已准备，且 workspace 验证并行。 | **未完成。** 这 20 次没有录音、ASR、LLM、控制器持久化或 UI 时段；194.804ms 不是 V11 的结果到派发或停止到派发全程 P95。仍缺 20 次同组 10 秒真人录音、规范 warm 状态、快捷键指示/Raw 提交/实际派发各段统计、冷加载和远程耗时。不得据此宣布 500ms/15 秒门槛通过或修改门槛。 |

## 本机开发包补充证据

[实际开发包记录](evidence/linux-development-package.json)：Arch `0.1.0dev20261105-1`（包版本日期按 UTC）已由 makepkg 生成，约 14 MiB，六程序归档字节核对通过。[真实包隔离 ALPM 验证](evidence/alpm-product-install-2026-11-06.json)确认安装 SHA/大小/模式、查询和卸载清理，宿主程序路径未变；两次 nodeps、无 scriptlet/外网，不能替代宿主依赖或桌面测试。[归档解出后的 release 启动](evidence/linux-package-runtime-startup.json)十项 CLI/单实例检查通过；[最终源码 debug DOM/IPC](evidence/tauri-ui-smoke-final.json)五项通过。当前系统未安装此包，未对外发布；这些补充证据不增加完整 V 项数量，仍为 1/11。

首次打包因 ldd 直接显示的动态加载器路径未被解析而停止。已最小修复并用六个真实程序重新审计所有直接依赖和包归属，通过后再打包；原失败与修复见 [依赖审计](evidence/package-library-audit.json)。未跳过缺失依赖门槛。

## 接续验收记录规则

新增记录须写明实际设备/系统、源码及二进制身份、模型完整 SHA-256、后端/线程、冷/暖状态、输入、步骤、结果和证据路径；A 的模拟 adapter 与 M 的真实桌面必须标明。只有该行全部条件满足才将整项改为通过；不得把 smoke、测试总数、脚本存在或一次正确输出扩大解释。后续发现失败应保留原日志并给独立修复后记录。

当前桌面已正常解锁，组件正向与焦点负例已有真实证据；完整真人录音流程、原生目标平台、经确认的真实远程服务与人工质量评分仍待完成。本轮不修改原规格或验收门槛，也不把组件成功扩大为全项通过。

[tests]: evidence/unlocked-workspace-tests.log
[clippy]: evidence/unlocked-workspace-clippy.log
[desktop]: evidence/m0-desktop-unlocked-2026-11-06-r3.json
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

[native]: evidence/native-input-runtime-final.json
[keyboard]: evidence/native-keyboard-verification.json
[ownpaste]: evidence/tauri-own-paste-native.json
[browserwl]: evidence/browser-paste-wayland-r4.json
[browserx]: evidence/browser-paste-xwayland.json
[paste20]: evidence/browser-paste-wayland-20.json
[focusguard]: evidence/desktop-focus-guard.json
[lifecycle]: evidence/worker-pool-lifecycle.json
