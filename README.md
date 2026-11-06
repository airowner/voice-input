# AI 语音输入桌面工具

按照 [V1 技术规格](specs/v1.md) 从 M0 开始实施。两引擎 CPU 构建、模型冻结、Linux Tauri 设置页和本地处理链路已实现并验证核心路径，尚未达到 V1 发布标准。桌面已解锁；源码已修复实际发现的 WebKit/XWayland 粘贴兼容问题，并通过专用框、浏览器和焦点守卫验证。修复版开发包已生成并验证，详见阶段记录。

- [阶段记录与下一步](docs/PROGRESS.md)
- [M0 实测记录](docs/M0.md)
- [V01–V11 验收记录](docs/ACCEPTANCE.md)
- [候选模型与源码冻结状态](docs/SUPPLY_CHAIN.md)

在有桌面会话访问权限的环境中运行：

```bash
python3 scripts/probe_m0.py --network --output docs/evidence/m0-next-preflight.json
lua scripts/check_hyprland_binding.lua
```

Python 探针只做读取；不录音、不改剪贴板、不注入按键、不重载配置。它只保存指定文件，不覆盖已有证据。`--network` 查询两个公开上游仓库的 Git HEAD，不下载模型或发送文本。

退出码 2 表示预检存在阻塞。退出码 0 也只表示预检通过：真实按键释放、剪贴板 owner 生命周期、受控粘贴、采集和引擎构建仍须完成，不能以预检代替 M0 或产品验收。Lua 检查使用模拟的 compositor 接口，仅验证本机 Omarchy helper 的参数传递。

当前工具版本直接从机器 PATH 获取，不替换为系统包中的其他版本。桌面默认 `Super+Shift+D` 静态发现 Docker 绑定，须先查询实际绑定再确定快捷键。

## 本机开发运行

依赖使用当前 PATH 中的 Rust、Node、CMake、Clang。系统构建依赖和打包边界见 [安装说明](docs/INSTALL.md)。首次准备：

```bash
npm ci
npm run build:workers
# 明确下载两个冻结模型，合计约 1.27 GB；应用内也可主动下载或导入
python3 scripts/prepare-models.py
npm run tauri dev
```

首次打开需导入/下载 ASR 和 LLM 模型、明确选择 LLM 模式后才能录音。设置页启动的录音进入预览；原文、处理结果和复制操作分开展示。远程模式需用户填写 HTTPS 端点、模型和凭据并确认上传。没有配置远程服务时，本地模式不会调用远程 LLM。

参考机可额外构建 AVX2 优化 worker，保留 portable CPU 基线：

```bash
python3 scripts/build-workers.py --profile avx2
npm run check
npm run tauri build -- --config src-tauri/tauri.avx2.conf.json
```

基础配置只打包 portable worker；优化配置同时打包两档，由宿主按 CPU 能力选择。开发及发布包的实际验证记录见 [验收矩阵](docs/ACCEPTANCE.md)；Arch 开发包命令、许可清单和稳定发布门槛见 [发布说明](docs/RELEASE.md)。Windows、macOS 和独立 X11 桌面尚未适配完成，不应使用本项目宣称跨平台 V1 已发布。


本机已生成并检查 Linux/Arch 开发包，位于 `dist/arch/linux-development/`；准确文件名和 SHA-256 见 [产物验证记录](docs/evidence/linux-development-package.json)。真实归档六程序核对、隔离 ALPM 安装/查询/卸载、归档解出后的启动与 CLI 已通过。隔离测试跳过依赖检查，不代表宿主桌面验收；未向外发布或安装到当前系统。完整验收当前仅 V05 A 通过。


当前修复版见 [新开发包验证记录](docs/evidence/linux-development-native-package.json)，实际文件位于 `dist/arch/linux-development-native/`。它包含已实测的原生 Ctrl+V 修复；旧 `linux-development/` 包仅保留历史验证，不包含此修复。运行与限制见 [安装说明](docs/INSTALL.md) 和 [原生输入决策](docs/NATIVE-INPUT.md)。
