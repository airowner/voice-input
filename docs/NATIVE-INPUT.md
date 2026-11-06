# Hyprland 原生粘贴适配决策

2026-11-06 在解锁的 Omarchy 4.0.6 / Hyprland 0.57.0 上复测，原规格的 wtype 固定 Ctrl+V 对 GTK4 Wayland 生效，但在 Tauri/WebKit 和 XWayland 输入框中没有可信粘贴事件。命令成功不代表目标收到输入，应用专用框的严格回读正确保持自动交付关闭。

同一 XWayland 目标的独立对照表明，标准 pc/us 键图及 evdev KEY_V=47、Control=4 可逐字粘贴。释放式快捷键的早期失败同样涉及动态键码与 compositor 物理布局解析不同；未修改 resolve_binds_by_sym 或用户桌面配置。

据此在授权任务范围内更换实现：应用直接使用 [virtual-keyboard-v1 协议定义](https://github.com/swaywm/wlroots/blob/master/protocol/virtual-keyboard-unstable-v1.xml)发送固定 Ctrl+V。外部行为保持“剪贴板纯文本、目标守卫、一次尝试、无 Enter、无自动重试”，原规格文件不改写；wtype 保留为能力诊断工具，不作为自动重发备用路径。

实现边界：

- libxkbcommon 在启动时后台准备固定键图，结果仅驻留内存。真正粘贴不等待编译，未准备好或失败时返回手动交付。
- 通过独立 Unix socket 和匿名 memfd 传键图，协议接收及诊断输出有界；无 root、uinput、额外 sidecar 或守护进程。
- 协议准备和最后焦点检查共享 500ms 截止时间。实际发输入之前再次检查原目标、焦点 epoch、连接和锁定状态，不激活旧目标。
- 输入未发出时失败为 Manual；任何输入可能已发出后失败为 Unknown。部分帧失败只关闭连接，清理不重复按键。
- 剪贴板回读和只读桌面命令统一为非阻塞、有界读取；超时回收自有进程组，避免后代持管道导致无界 join。

真实证据见 [本轮汇总](evidence/native-input-runtime-final.json)：Tauri 专用框、Chromium Wayland/XWayland、GTK 两后端正向交付及焦点/关闭负例。20 次浏览器交付组件 P95 约 195ms，不含录音、模型、UI 和控制器落盘，不是完整 V11。

当前仅证明本机已记录配置；其他 compositor、键盘布局和任意应用控件不能据此视为支持。每次应用会话仍须专用框实际可信事件与逐字回读才启用自动尝试，敏感应用/终端继续手动，用户仍可选择始终预览。完整 V01/V02/V11 仍按验收矩阵补齐，不用组件结果替代真人录音与人工复核。
