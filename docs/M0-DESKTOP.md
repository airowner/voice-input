# M0 桌面能力探针

本记录仅用于第十一节 M0 的真实桌面能力验证，不代表第十节 V01–V11 整体验收。源规格为 `specs/v1.md`。

## 当前状态：解锁后已有真实输入证据

桌面锁定限制已解除。当前 [M0 r3](evidence/m0-desktop-unlocked-2026-11-06-r3.json)的配置、socket2、剪贴板 owner、短时采集和虚拟键释放均通过；原 wtype 的 XWayland 粘贴仍失败，因此该诊断脚本仍返回部分未通过，不能删掉失败或把它改成通过。随后生产适配的 [Tauri 专用框](evidence/tauri-own-paste-native.json)、[Chromium Wayland](evidence/browser-paste-wayland-r4.json)与[XWayland](evidence/browser-paste-xwayland.json)均取得可信 paste/input 与逐字一致回读。GTK 生产适配补测尚未在本快照登记；这些证据不含完整录音链路。

| 当前输入路径 | 实测结果与范围 |
|---|---|
| 原 wtype → GTK4 Wayland | M0 合成文本与 owner 生命周期通过 |
| 原 wtype → GTK4 XWayland | 窗口启动/聚焦/clipboard 回读通过，粘贴 buffer 回读超时；原失败保留 |
| 原 wtype → Tauri/WebKit Wayland | 没有可信 paste/input，typed 确认为 mismatch，自动交付保持禁用 |
| 标准物理键码独立诊断 → 同一 GTK XWayland | 重置 buffer 后逐字通过；属于诊断，不是原工具或生产适配通过 |
| 内部原生 Ctrl+V → Tauri 专用框 | 可信事件、逐字回读、本会话能力确认通过 |
| 内部生产适配 → Chromium Wayland/XWayland | 两后端逐字通过；Wayland 20 次合成粘贴全部通过，非 20 次录音 |
| 生产目标守卫 → 两个自有 Wayland 窗口 | 固定结果至少 2 秒延迟内切走/切回及关闭目标均 Manual，buffer 不变；非完整 V02 |

完整汇总与运行库版本见 [native-input-runtime.json](evidence/native-input-runtime.json)。本轮局部 adapter 调用 P95 194.804ms 仅含 copy、guard、协议 ACK，未含控制器/UI/音频/模型；正式 V11 仍待验。

## 执行入口

在已解锁的 Omarchy / Hyprland 用户会话运行：

```bash
python3 scripts/m0_desktop_probe.py --output docs/evidence/m0-desktop-YYYY-MM-DD.json
```

输出文件必须不存在。退出码 0 表示已执行的桌面能力探针通过；2 表示部分未通过，具体看 JSON 中每一项的 status、reason 和 cleanup。运行时会短暂显示两个自有测试窗口并切换其焦点，原窗口焦点在清理时恢复。不得在屏幕锁定或用户需要连续键盘操作时运行。

## 探针行为与证据边界

| 检查 | 方法 | 通过证据 | 不覆盖的验收 |
|---|---|---|---|
| 配置格式 | 读取 `hyprctl version/configerrors` | 精确版本及无配置错误 | 安装包中的配置部署 |
| 快捷键冲突 | 解析真实 `hyprctl -j binds` | Super+Shift+D 冲突数、候选键空闲 | 用户最终快捷键选择 |
| release Lua | runtime `hl.bind`，标准物理布局一致的 virtual-keyboard 合成按住/释放 | 按住中无回调、释放后恰好一次，绑定移除 | 真人物理键长按、应用 toggle 防抖 |
| 剪贴板 | `wl-copy --foreground` owner 保活，wl-paste 回读，替换 owner | 合成 UTF-8 字节一致、旧 owner 回收 | 应用退出时 clipboard manager 的缓存策略 |
| Wayland / XWayland | 每种 GDK backend 的自有 GTK4 TextView 分别测试原 wtype；X11 另做标准 KEY_V 独立诊断 | 后端标记及完整 buffer hash，原路径与诊断各自报告；只有相应路径实际匹配才通过 | 生产 GTK 适配、应用完整处理链和 20 次录音；浏览器另有独立证据 |
| socket2 | 订阅当前 compositor socket2 | 记录接收到的事件名称，activewindowv2 可见 | 控制器在事件断线时的失效策略 |
| 音频 | `pw-record` 默认来源，16 kHz mono s16，读满 8000 样本后主动结束 | 字节仅驻留内存、无音频文件 | cpal 实现、设备拔出、中英文 ASR 和声音质量 |

程序只向新建且 PID、窗口地址匹配的自有测试窗口发送按键，不向运行前窗口粘贴。每次注入前再次检查焦点；焦点检查和系统注入仍存在操作系统竞态，与规格第 4 节所述一致。

## 剪贴板恢复和临时状态

原剪贴板所有声明格式的字节均只保存在内存，不写入命令行或证据文件。每格式、全部格式总量上限为 8 MiB，格式数最多 32；超限或无法完整读取时跳过剪贴板写入与粘贴探针。原剪贴板为空时运行后清空；非空时通过 GTK4 `Gdk.ContentProvider` 联合内容恢复所有格式，并逐格式回读比较。恢复后的 GTK owner 等到下一次剪贴板 ownership 变化自行退出，避免在没有 clipboard manager 时过早丢失原内容。证据只记录恢复结果、格式数量，不记录内容。

临时 release 绑定仅由 `hyprctl eval` 注册，不修改 `~/.config/hypr/`，不 reload 桌面。清理使用 `hl.unbind` 移除本次添加的组合，并通过实际 binds 确认不存在探针描述。自有测试窗口和 wl-copy 子进程最终关闭。

## 可用 Lua 片段

默认 Super+Shift+D 在此机 Omarchy 默认配置中用于 Docker，最终采用的组合必须以本次真实 binds 检查为准。探针使用如下模板（把键替换为证据中的空闲项）：

```lua
o.bind("SUPER + CTRL + SHIFT + F12", "Voice input",
  "/usr/bin/voice-inputctl toggle", { release = true })
```

这份片段的执行文件路径必须替换为实际安装路径；探针回调使用临时 marker，不能作为 voice-inputctl 产品验收。

## 执行记录

- 子代理受限环境：Hyprland、Wayland、PipeWire、DBus socket 连接均被拒绝；因此子代理只做 Python AST、bounded read、连续 JSON 回复处理的无桌面 smoke 验证，均通过。
- 真实桌面执行由主代理完成，准确软件版本、每项结果和清理状态以 `docs/evidence/m0-desktop-*.json` 为准。
- Windows、macOS、X11 独立桌面尚不在此脚本验收范围；这里的 x11 backend 验证的是 Hyprland 下的 XWayland。

### 首次真实运行与修正

`docs/evidence/m0-desktop-2026-11-06.json` 实测：Hyprland 0.57.0（commit `0aa6745f705f2c4b0a5004eb019ef76e5bbdb426`），Omarchy 4.0.6-1，wl-clipboard 2.3.0，wtype 0.4-2，GTK4 4.24.1，PipeWire 1.6.10，Python 3.14.9。配置无错误、socket2 收到 activewindowv2/openwindow，参考键 Super+Shift+D 确有一项冲突，Super+Ctrl+Shift+F12 空闲。

首跑的旧 `hyprctl dispatch focuswindow` 在此版本返回 7，故两种目标的粘贴未执行，不能标为通过。探针现改用本机 `/usr/share/omarchy/bin/omarchy-hyprland-focus-app:33` 已使用的准确接口：

```bash
hyprctl dispatch 'hl.dsp.focus({ window = "address:0x..." })'
```

首跑音频按 `--sample-count` 自结束的退出码异常，未据此报通过。现改为无 sample-count 的流，读取恰好 16000 字节内存后，由探针显式终止 recorder；JSON 明确记录人为终止，不把进程退出码当作采集证据。

新版官方文档入口：[Dispatchers](https://wiki.hypr.land/configuring/core/dispatchers/)、[hyprctl](https://wiki.hypr.land/configuring/core/advanced-configuration/using-hyprctl/)、[Binds](https://wiki.hypr.land/configuring/core/binds/)。实现的已安装版本语法证据仍以本机 Omarchy 脚本为准。

### 早期焦点失败的已确认原因（锁屏历史）

主代理进一步确认 compositor 的 `hyprctl locked` 为 `true`，`hyprctl -j locked` 为 `{"locked":true}`，即使 logind 的 `LockedHint=no`。因此不得仅以 logind 判断桌面已解锁，也不得把 DPMS on 当成解锁。此前 DPMS 恢复后的焦点探针仍失败，不能将 DPMS 单独归为根因。

探针现于开始时读取并记录 compositor_locked。若为 true 或查询无法确认，两种后端明确跳过创建窗口与按键测试，原因分别为 compositor_locked/compositor_lock_unknown；程序不会唤醒、解锁或修改用户配置。须由用户正常解锁后重新执行，才能获得粘贴及 release 的真实证据。

`voice-platform` 的自动交付守卫同样同时要求 compositor unlocked、logind Active/LockedHint 正常、monitor DPMS on、同一目标、epoch 未变及事件流持续连接。后台监控观察到锁定、解锁、查询失败或连接断开会使旧目标失效。锁屏状态查询与系统注入之间仍存在 OS 竞态，不能承诺原子交付。


### 解锁后的键码兼容性定位与生产修复

初次解锁探针里，release 注册正确但 wtype 释放回调为 0；标准物理布局一致的 F12 虚拟事件重测后，按住无回调、释放恰好 1 次。此机绑定按物理布局解析，而 wtype 动态键码与该解析不一致；未为测试更改 `resolve_binds_by_sym` 或个人配置。所有释放结论仍明确是合成虚拟事件，不能当真人按键。

XWayland 初报 `owned_target_reply_timeout` 时未区分阶段，后续仅增加阶段/受控 GTK 诊断，成功门槛未放宽。r3 将失败定位到 `paste_and_buffer_reply`；同一自有目标重置后，标准 pc/us KEY_V=47+Ctrl=4 独立诊断逐字通过。Tauri/WebKit 专用框随后也证明原 wtype 没有可信输入，不能只因工具返回 0 就启用自动交付。

生产适配据此改为内部标准 Wayland virtual-keyboard-v1 Ctrl+V，实现保留剪贴板回读、最后目标/焦点检查、一次 attempt 和无自动重发。键图后台准备，键入流程不等待编译；本次协议和最后守卫共用 500ms deadline，未发输入失败手动，可能发过输入失败 Unknown。只读命令使用有界非阻塞读，超时清理未 reap 的自有进程组，保留诊断 128KiB / 剪贴板 4MiB 上限。详见 [实现验证](evidence/native-keyboard-verification.json)。未增加 root/uinput、守护进程或 sidecar，也未修改原 V1 规格。

[专用框成功证据](evidence/tauri-own-paste-native.json)沿真实 typed UI 路径，脚本不把预期文字赋给输入框，必须观察可信 paste/input、实际逐字回读和确认后的能力状态。浏览器测试使用隔离 profile 与自有 textarea；生产 adapter 返回 SentUnconfirmed，随后该固定控件的实际回读通过，不扩大为所有应用都有消费回执。

[真实焦点负例](evidence/desktop-focus-guard.json)记录 A→B→A 的 epoch 改变、至少 2 秒固定合成结果延迟，以及关闭目标；均无粘贴且合成结果可访问，原窗口焦点已恢复。它不涵盖完整应用中的真实 provider、锁屏/IPC 断线或全部 V02 条件。新生产构建/包另行验收，本文不沿用旧包检查来证明已包含本次修复。
