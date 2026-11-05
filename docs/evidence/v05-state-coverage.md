# V05 状态覆盖映射

日期：2026-11-06。范围：`crates/core/src/controller.rs` 的确定性 Action/Effect 合同和 `crates/runtime/tests/recovery_crash.rs` 的真实进程故障恢复。**V05 A 已通过。** 最终运行证据见 [workspace-tests-final.log](workspace-tests-final.log)：core 22 项及 crash 2 项（含子进程 helper）全部通过，含下列三项新增边界测试；[workspace-clippy-final.log](workspace-clippy-final.log)亦通过。实际逐项断言与三阶段真实 SIGKILL 共同闭合 V05 的软件合同，不扩大为桌面 M 验收。

| 规格 §4 状态 / 条件 | 测试入口 | 明确断言 / 范围 |
|---|---|---|
| Ready / Result → Recording；冻结配置 | `raw_commit_gate_busy_snapshot_and_cancel_late_results`；新增 `recording_toggle_freezes_target_and_repeated_stops_never_restart_asr` | 配置变更不影响当前任务；Ready/Result Toggle 仅发一个 StartCapture，新 job_id/generation；重复 Start 不另开采集 |
| Recording → Transcribing；停止 / 再切换 / 到限 | `raw_commit_gate_busy_snapshot_and_cancel_late_results`；新增 `recording_toggle_freezes_target_and_repeated_stops_never_restart_asr` | Stop/Toggle 发一个 StopCaptureAndTranscribe，冻结目标；三次重复 Stop 无新效果且不改目标。到限由宿主发送同一 Stop；计时器是否实际触发不由纯 core 测试证明 |
| Recording / Transcribing 取消 → Ready | 新增 `cancellation_and_busy_toggle_at_every_async_boundary_have_no_replay`；`cancelling_before_raw_discards_late_asr_and_recovered_attempts_only_preview` | Capture/Asr 边界取消只发 CancelWork，清当前任务、无历史；晚到原文和各阶段提交回调 Invalid，无效果、快照不变。已有 Raw 的 RawCommit 边界取消保留原文进入 Result |
| Transcribing 非空 → 先 Raw 提交 → Rewriting；提交失败预览 | `raw_commit_gate_busy_snapshot_and_cancel_late_results`；`persistence_failure_preserves_raw_and_cannot_trigger_llm` | RawCommitted 前不能 Rewrite；持久化失败保留完整原文且无 LLM 效果；配置快照不变 |
| Transcribing 静音 / 空文本 / 失败 → Result | `empty_asr_and_invalid_output_never_proceed_and_tick_removes_expired_text`；新增 `failed_at_every_async_boundary_preserves_raw_and_rejects_late_callbacks` | 空文本不发后续效果；直接 Asr Failed 取消本次任务、无 Rewrite/native 效果。声学静音门控在 audio/runtime 测试，不由空字符串测试冒充 |
| Rewriting 成功 → 提交结果 → Delivering / Result | `ui_and_retry_always_preview_and_old_generation_cannot_complete_retry`；`auto_delivery_requires_attempt_commit_and_is_dispatched_once` | UI / Retry 无自动交付；快捷键仅在 ProcessedCommitted 后准备交付 |
| Rewriting 取消 / 超时 / 崩溃 / 无效结果 → Result | 新增两项各边界 Failed / Cancel 测试；`empty_asr_and_invalid_output_never_proceed_and_tick_removes_expired_text` | Raw 逐字保留；ProcessedCommit 时保留独立结果；只取消/记状态，不重新提交 provider。超时/worker故障由宿主映射 Failed，测试不伪称实际杀过推理引擎 |
| Delivering 有效 → durable attempt → 一次 native → Result | `auto_delivery_requires_attempt_commit_and_is_dispatched_once`；`processed_commit_guard_and_attempt_failures_cannot_paste` | AttemptSaved 成功前不发 Deliver；重复 attempt / Delivered 回调 Invalid；失败不能绕过提交门槛 |
| Delivering 不安全 / 失败 / 取消 → 手动 / 预览 / Unknown | `processed_commit_guard_and_attempt_failures_cannot_paste`；`cancellation_after_native_dispatch_is_unknown_and_cannot_replay`；新增各边界 Failed / Cancel 测试 | guard=false 手动；Prepare/AttemptCommit 失败保留内容且无 native 效果；已派发 native 时 Failed/Cancel 为 Unknown，晚到成功不改结果。真实焦点/锁屏/权限由 V02 M 验证 |
| Result 重试 → Rewriting，固定预览 | `ui_and_retry_always_preview_and_old_generation_cannot_complete_retry`；`oversized_raw_is_retained_but_never_submitted_to_provider` | 同 Raw、新 generation、无目标、固定预览；旧 generation 结果不生效；超限不调用 provider |
| 任意退出 / 崩溃恢复 → 已提交记录、Unknown、不重放 | `recovery_crash` 集成测试的父测试；`cancelling_before_raw_discards_late_asr_and_recovered_attempts_only_preview` | 三阶段真实 SIGKILL：Raw 后、attempt 后/native 前、模拟 native 后/result 前；重启原文逐字保留，attempt Unknown，Restore/Tick effects 空。native 是临时计数文件，不是桌面键盘；退出时真实引擎清理由 V07 M 另验 |

## 新增测试的异步边界枚举

`Capture`、`Asr`、`RawCommit`、`Rewrite`、`ProcessedCommit`、`Prepare`、`AttemptCommit`、`Delivery` 共八处单独构造并检查，不以一个“大阶段”代替其他等待窗口。

- `failed_at_every_async_boundary_preserves_raw_and_rejects_late_callbacks`：先验证异 job_id / generation 的 Failed 完全无效，再直接注入当前 token 的 Failed；效果严格为一次 CancelWork，已有 Raw 时另加一次状态持久化。逐字保留已有 Raw / processed；只有已派发 native 的 Delivery 为 Unknown；重复 Failed、晚到 Raw/Rewritten/Delivered 均无效果且完整快照（含 sequence）不变。
- `cancellation_and_busy_toggle_at_every_async_boundary_have_no_replay`：全部八边界取消；Asr 起所有工作阶段 Start/Toggle 返回 Busy 且快照不变；重复 Cancel 无工作效果；所有成功类回调在取消后均 Invalid。已有 Raw 时换 generation 并保留预览，尚无 Raw 时回 Ready。
- `recording_toggle_freezes_target_and_repeated_stops_never_restart_asr`：Ready Toggle 开始、录音期重复 Start 幂等、再次 Toggle 停止并冻结目标、重复 Stop 不重复 ASR、失败后 Result Stop 幂等、Result Toggle 生成新任务。

本表是可复核的合同映射，不声称穷举所有输入排列，不涵盖真实音频句柄关闭、前端响应、OS 注入或远程请求撤回。本次统一测试日志已通过并关联上述测试入口，因此验收矩阵中的 V05 标为通过 A；后续修改需重新验证，不能沿用本快照自动认定。
