# 公开语料的本地开发执行器

`crates/runtime/examples/corpus.rs` 读取 `tests/fixtures/v10/manifest.json` 的 15 条素材，通过真实生产 `WorkerPool`、`rewrite::messages` 和 `validate_output_with_config` 执行本地流程。此执行器不会访问麦克风、剪贴板、系统密钥库、外部 TTS、远程 LLM 或模型下载。尚未安排实际推理时，只编译或使用 `--dry-run`。

## 文件和门槛复核

```bash
python3 tests/fixtures/v10/prepare.py --verify-only
cargo check -p voice-runtime --example corpus --locked -j 1
cargo run -p voice-runtime --example corpus --locked -j 1 -- \
  --dry-run --output docs/evidence/v10-corpus-dry-run.json
```

输出文件必须是新路径，已有文件不会覆盖。`--dry-run` 只重算项目模型/worker/语料文件 SHA-256、校验 WAV 和运行本地语音门控；不启动推理进程、不开恢复库、不生成 ASR/LLM 文本。模型仍须预先存在于 `models.lock.json` 指定位置；程序不会自动取得权重。

## 经安排后实际运行

```bash
cargo run -p voice-runtime --example corpus --locked -j 1 -- \
  --output docs/evidence/v10-corpus-local.json
```

默认优先通过 `worker_path` 选择本机支持的 `build/workers-avx2` 可执行文件，否则使用 `build/workers`；可用 `--workers DIRECTORY` 显式指定可信本机构建。CPU 固定 4 线程、Base 多语 ASR、锁定的 Qwen 1.5B Q4 本地 LLM、`cleanup` 处理档案，交付固定预览，不注入任何窗口。

两套模型在执行前再次核对锁定的完整 SHA-256/大小。池先执行仅加载的 `preload`，记录各自加载耗时，然后依次复用驻留 worker；没有生成式 warmup，也没有自动重试。同一条失败后保留原文并继续下一条。worker 因错误被生产池关闭时，下一条可能包含重新加载时间，报告会明确标记，不能把这类时长当作纯 warm 推理性能。

临时 SQLite 使用生产恢复实现，**每条 ASR 原文先事务提交成功，才构建提示和请求本地 LLM**。原文提交失败会跳过该条 LLM；模型响应结构/字面量校验不合格不会放宽规则或改写断言。JSON 响应、原文和已验证结果只进入本次显式要求的开发报告。

所有 WAV 先经过 `voice_audio::contains_voice`；全零静音应记录 `skipped_no_speech`，ASR/LLM 请求均为零。此执行器没有远程调用分支；这属于静音的**软件路径证据**，没有网络抓包审计，也不替代真实桌面或供应商接口验收。

如只排查前几条，可显式指定 `--limit 1` 至 `--limit 15`。报告会标记 `partial_run`，部分运行不算完成 15 条。

## 报告含义与限制

报告按条记录参考文本、实际 Raw、处理结果、原始本地 JSON 响应、结构/受保护字面量校验结果、各阶段耗时、缓存状态及错误 code。每条完成后原子更新报告，已完成条目不会因为后续失败而被改成通过。

临时恢复库遵守产品的最多 5 条/24 小时合同，正常退出后删除；异常终止可能遗留临时测试库，不会自动恢复或重放。本次开发报告有意保留全部**公开授权**素材的结果，便于复核，绝不能把这种记录方式接入默认产品日志或用于真实用户录音。进度 stdout 只包含样本编号、状态、耗时和错误码。

汇总只统计请求、原文保存、结构校验和静音跳过；始终包含 `manual_review: pending`、`remote_mode_run: false` 与 `v10_acceptance: pending`。不计算或捏造人工可用率，不将结构校验等同语义保真，不声称跑过远程模式，也不把文件级执行时间当作 V11 麦克风/快捷键端到端 P95。

退出码 0 仅表示执行器写完逐条报告；样本失败仍写入各自的 `error_at`/`error.code`，必须检查报告，不代表样本或验收通过。每条缓存标签实时取自生产池 `is_loaded(signature)`：`resident expected` 或 `cold/recreated`，不在错误后的重建条目上继续标记 warm。

自然口头禅、口述路径、自然单人中英混输等缺口继续见 [语料说明](V10-CORPUS.md)。要完成 V10，仍需补齐素材、实际跑两种 LLM 模式并由人核对受保护事实和可用性。

## 本轮隔离网络执行

本机允许普通用户创建 user/network namespace。`docs/evidence/offline-corpus-runner.py` 仅运行固定的本地 corpus 二进制，以 `unshare --user --map-root-user --net` 启动。脚本用内核接口核对命名空间内只有未启用的 loopback（不以宿主挂载的 sysfs 网卡清单判断），向 RFC5737 文档保留地址的连接探测返回 ENETUNREACH，然后执行 15 条本地语料并记录执行前后该命名空间的路由和 TCP/UDP 表。

该运行无法使用外网，但没有实际录音或粘贴，也没有重新联网后的后台连接审计，因此只补充 V03 的离线推理证据。并行开发编译/worker 冒烟会影响计时，不用于 V11。结果见 `v10-corpus-local-offline.json`、`v10-offline-network.json` 与 `v10-corpus-offline-run.log`；必须核对逐条错误，不能把执行器退出 0 当作全部样本通过。
