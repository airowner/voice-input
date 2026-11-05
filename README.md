# AI 语音输入桌面工具

按照 [V1 技术规格](specs/v1.md) 从 M0 开始实施。两引擎 CPU 构建、模型冻结及基础探针已完成，Linux 桌面应用正在集成，尚未达到 V1 发布标准。当前 compositor 锁屏，自动交付保持禁用，详见阶段记录。

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
