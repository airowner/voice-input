# Linux / Omarchy 安装与卸载

当前构建目标为 Omarchy / Hyprland x86-64。开发验证包不是 V1 全项验收通过的发行版；Windows、macOS、其他桌面与 GPU 支持不能据此视为已验证。具体限制以包内 `release-manifest.json` 为准。

## 安装已构建的 Arch 包

先检查随包的 `archive-manifest.json`，核对实际包文件的 SHA-256：

```bash
sha256sum /实际路径/voice-input-版本-1-x86_64.pkg.tar.zst
sudo pacman -U /实际路径/voice-input-版本-1-x86_64.pkg.tar.zst
```

文件名及压缩格式以清单中的 `archive` 为准，不要把示意文件名直接当作实际产物。打包工具不安装软件、不自动安装依赖、不执行 sudo；上面的安装操作由用户在自己的终端执行。依赖由本机实际 ELF/ldd 与 pkg-config 结果生成，并包括桌面、音频、剪贴板和通知工具。当前 Hyprland 参考机的 `wtype` 动态键码在 WebKit/XWayland 上未产生有效粘贴，应用已改用同一 Wayland virtual-keyboard 协议的内置 Ctrl+V 适配。它只发送固定 Ctrl+V，使用 libxkbcommon 和系统 xkeyboard-config 数据，不需要 root、uinput 或额外守护进程。仅准备好键图不证明 compositor 能力，仍须专用框真实回读；wtype 保留为诊断工具。

升级前先停止录音并在托盘中退出旧版应用，避免仍驻留的旧进程与新文件混用。

安装路径：

- `/usr/bin/voice-input`、`/usr/bin/voice-inputctl`。
- `/usr/bin/voice-asr-worker`、`voice-llm-worker`；显式包含优化档的包另有两个 `-avx2` 版本。
- `/usr/share/applications/voice-input.desktop` 与 hicolor PNG 图标。
- `/usr/share/doc/voice-input/`：规格、版本/验证范围、源码/依赖/模型来源清单、快捷键示例。
- `/usr/share/licenses/voice-input/`：项目、两引擎、nlohmann JSON 与目录模型的许可副本。

包含 AVX2 档的包仍同时保留基线。应用只有在 AVX2/FMA/F16C/BMI2 特征和对应资源均可确认时才选择优化档；旧 CPU 使用基线，不要求本机生成优化资源，也不应手动把优化 worker 改名覆盖基线。是否包含优化档可读取清单的 avx2_workers_bundled。

## 首次启动

从应用菜单启动“声笺”，或在真实图形登录会话运行：

```bash
/usr/bin/voice-input
/usr/bin/voice-inputctl status
```

先完成麦克风、ASR 模型、LLM 模式三项检查。包不包含模型权重，也不会安装或启动时自动下载模型。按设置页明确选择导入或下载、选择本地/远程模式；远程端点和凭据需要用户确认。缺少 Secret Service 时仅使用本次会话密钥，不写入明文配置。

开发包的录音、处理和交付能力必须结合实际诊断使用。自动粘贴未通过实机验证时使用预览和主动复制。锁屏期间不进行录音/按键验收，不通过命令绕过锁屏；正常解锁后再测试。本轮已在解锁状态验证可信粘贴/逐字回读和焦点/关闭守卫；首次运行仍需点击设置页“验证本次会话的粘贴能力”，验证前只保留预览/手动交付。

CLI 仅连接当前登录会话的私有接口，不会在应用未运行时偷偷启动应用：

```bash
/usr/bin/voice-inputctl start
/usr/bin/voice-inputctl stop
/usr/bin/voice-inputctl toggle
/usr/bin/voice-inputctl cancel
/usr/bin/voice-inputctl status
```

socket 实际位于 `$XDG_RUNTIME_DIR/voice-input-<会话哈希>/control.sock`，私有目录 0700、socket 0600，按当前会话标记和用户隔离。CLI 与应用应在同一用户、同一图形登录会话运行；应用未运行或会话变量缺失时返回可读错误。

## Hyprland 快捷键由用户自行加入

先读取真实绑定，选择空闲组合：

```bash
hyprctl -j binds
```

此机默认 `Super+Shift+D` 已用于 Docker，不应直接覆盖。M0 读取时 `Super+Ctrl+Shift+F12` 空闲，但仍须在安装当天重查。示例位于 `/usr/share/doc/voice-input/voice-input-bindings.lua.example`：

```lua
o.bind("SUPER + CTRL + SHIFT + F12", "Voice input",
  "/usr/bin/voice-inputctl toggle", { release = true })
```

这段 Lua 的 release 选项已与本机 Omarchy helper/API 核对，实际 compositor 的虚拟物理布局事件已验证“按住不触发、释放恰好一次”。这不是用户物理按键或 20 次录音的验收；那些仍须独立执行。用户确认组合空闲后，自行加入个人 `~/.config/hypr/bindings.lua`，然后验证：

```bash
hyprctl reload
hyprctl configerrors
hyprctl -j binds
```

若已有冲突，选择另一个空闲组合；不要用包安装脚本解除或覆盖原绑定。应用先启动，再验证一次开始、一次停止和取消，且确认没有重复任务。包不写用户 Hyprland 配置、不安装自动启动项、不改系统服务。

## 卸载

先停止录音并从应用托盘退出，再运行：

```bash
sudo pacman -Rns voice-input
```

这只移除 pacman 管理的程序及文件，不删除个人数据、模型或系统密钥。卸载前可在应用中清空结果、删除不再需要的模型和凭据。应用数据在 Tauri 的用户应用数据目录，通常为 `${XDG_DATA_HOME:-~/.local/share}/local.voice-input.desktop/`；以实际运行环境为准。若曾自行加入快捷键，卸载前后自行移除那条个人绑定并复查配置，避免留下指向已删除 CLI 的命令。

归档内容校验不能代替真实安装/卸载验收。只有相应的实际记录存在，才可将该步骤标记通过。
