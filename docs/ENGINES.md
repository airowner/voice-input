# M0 引擎 CPU 构建

范围：仅验证固定上游源码可在参考机构建为静态 C API 库和命令行工具。它们不是规格第 7 节的应用 sidecar；上游 CLI 本身不提供本项目要求的长度前缀协议、取消监督或父进程退出清理合同。

2026-11-06：两引擎 CPU 静态库和 CLI smoke 均已实际构建通过。后续独立实现的应用 sidecar 及协议验证见 `WORKERS.md`，不能把本节的上游 CLI 与 sidecar 混为同一产物。

## 固定源码

| 引擎 | 官方仓库 | 完整提交 | 许可 |
|---|---|---|---|
| whisper.cpp | https://github.com/ggml-org/whisper.cpp | `b1535f5fb163f4443804888f7be980621226cb15` | MIT |
| llama.cpp | https://github.com/ggml-org/llama.cpp | `45b6604b20f17b7d7638042306c0cd1b6af5999c` | MIT |

2026-11-06 已从这两个官方仓库取得源码并检查工作树没有已跟踪文件修改。源码位于 `vendor/whisper.cpp`、`vendor/llama.cpp`，已由项目忽略规则排除。原始 LICENSE 副本保存在 `docs/evidence/engines-*-LICENSE.txt`，许可文件 SHA-256 随构建证据记录。引擎内含的其他第三方组件许可仍需在 M5 形成完整分发清单。

## 重建

```bash
python3 scripts/build-engines.py --evidence docs/evidence/engines-next-build.json
```

脚本从 `engines.lock.json` 读取完整提交、构建选项和目标；缺失的源码会按固定提交从官方仓库取得，已有源码必须匹配提交且无已跟踪文件修改，脚本不会覆盖不匹配的源码。构建严格串行，每次最多 2 个编译任务，避免占满此参考机的内存。证据 JSON 和同名日志文件拒绝覆盖。

配置使用机器 PATH 中的 CMake、Ninja 和默认 `cc`/`c++`，实际版本写入 JSON。发布时仍需固定工具链环境与目标系统 ABI；本次成功构建不能替代 Windows/macOS 或较旧 Linux 的兼容性测试。

`x86_64_cpu_portable` 配置关闭 `GGML_NATIVE`、SSE4.2、AVX、AVX2、FMA、F16C、BMI2 和 GPU 后端，以 x86-64 的编译器基线生成 CPU 构建；静态链接两个项目各自的 ggml 库，不混合两套符号。此配置侧重可移植性，性能门槛须另行实测，不能假定达到 V11。后续优化档必须独立命名、记录精确 ISA，并与基线分开验收。

## 构建产物及接口入口

| 引擎 | 头文件 | 主要静态库 | M0 CLI |
|---|---|---|---|
| whisper.cpp | `vendor/whisper.cpp/include/whisper.h`、`vendor/whisper.cpp/ggml/include/` | `build/whisper-cpu/src/libwhisper.a`，以及 `build/whisper-cpu/ggml/src/` 下的 ggml 静态库 | `build/whisper-cpu/bin/whisper-cli` |
| llama.cpp | `vendor/llama.cpp/include/llama.h`、`vendor/llama.cpp/ggml/include/` | `build/llama-cpu/src/libllama.a`，以及 `build/llama-cpu/ggml/src/` 下的 ggml 静态库 | `build/llama-cpu/bin/llama-completion` |

本次固定的 llama.cpp 已把 `llama-cli` 与 server 实现关联。M0 选用不启用 server 的 `llama-completion` 作为命令行推理工具，关闭 `LLAMA_BUILD_SERVER`、APP/UI、OpenSSL 和 subprocess；CLI 只用于开发 smoke，不作为发布 sidecar。`llama-common` 仍包含 HTTP 下载代码和 cpp-httplib，因此不能声称这个 CLI 无网络能力。发布 worker 应只链接 libllama 和 ggml，不链接 common/CLI；运行期网络边界还需 V03 审计。

Whisper C API 入口为 `whisper_context_default_params`、`whisper_init_from_file_with_params`、`whisper_full_default_params`、`whisper_full`、`whisper_full_get_segment_text`，支持 full params 的 `abort_callback`。Llama C API 入口为 `llama_model_default_params`、`llama_model_load_from_file`、`llama_context_default_params`、`llama_init_from_model`、`llama_tokenize`、`llama_decode` 和 sampler 接口。实现时应以固定头文件为准，不照搬旧版 API 示例。

构建完成状态、耗时、CLI smoke、产物 SHA-256 及完整命令以 `docs/evidence/engines-2026-11-06.json` 为准。模型加载/转写/生成由模型基线证据独立记录，不能由 `--help` 或 `--version` 代替。
