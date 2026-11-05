# M0 首批模型冻结与验证

日期：2026-11-06。固定清单为仓库根目录 `models.lock.json`。两个文件均已真实下载，按官方 Hugging Face API 的 LFS SHA-256 逐字节计算本地哈希并比对；大小、格式 magic 也通过。下载不启用或修改用户的 LLM 配置。

| 模型 | 固定来源 revision | 字节数 | SHA-256 |
|---|---|---|---|
| Whisper Base 多语，ggml-base.bin | `5359861c739e955e79d9a303bcbc70fb988958b1` | 147951465 | `60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe` |
| 官方 Qwen2.5-1.5B-Instruct GGUF Q4_K_M 单文件 | `91cad51170dc346986eccefdc2dd33a9da36ead9` | 1117320736 | `6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e` |

本机路径分别为 `models/whisper/ggml-base.bin` 和 `models/gguf/qwen2.5-1.5b-instruct-q4_k_m.gguf`。模型权重目录不提交 Git。后者是真实单文件 GGUF，不需要拼接分片。

## 许可与来源证据

- Whisper 转换模型库：[ggerganov/whisper.cpp](https://huggingface.co/ggerganov/whisper.cpp)。固定 revision 模型卡标注 MIT；OpenAI Whisper 固定提交 `86098128c0b4f24f0e2aa2994de830614b474227` 的许可文件副本在 `docs/evidence/models-whisper-LICENSE`，权重许可范围见 `models-whisper-license-scope.json`。
- Qwen 官方模型库：[Qwen/Qwen2.5-1.5B-Instruct-GGUF](https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF)。固定 revision 的 Apache-2.0 许可全文和模型卡在 `docs/evidence/models-qwen-LICENSE`、`models-qwen-README.md`。模型卡声明中英等多语支持；这不替代本应用 V10 质量验收。
- 原始带文件大小和 LFS SHA-256 的 API 响应分别在 `models-whisper-upstream.json` 和 `models-qwen-upstream.json`。完整本地核验结果在 `models-verification.json`。

发布时应随模型分发保留适用许可及归属；本次仅作本地开发模型准备，不代表已经产出或发布应用安装包。

## 重复执行

```bash
python3 scripts/prepare-models.py
python3 scripts/prepare-models.py --verify-only
```

该开发工具只按固定来源显式下载，既有完整文件先校验，不重复下载；下载使用同目录 `.part`，按固定大小限制写入，刷盘、完整 SHA-256 与 magic 校验后原子替换，错误/取消清理未完成文件；空间不足提前拒绝。HTTPS 重定向仅允许 Hugging Face 域名及其 `.hf.co` CDN，不使用 API Key 或代理环境变量。不提供应用运行时自动下载能力。

## ASR 冒烟 fixture

`tests/fixtures/jfk.wav` 来自 whisper.cpp 固定提交 `b1535f5fb163f4443804888f7be980621226cb15` 的公开 samples，16 kHz、16 位、单声道、11 秒、352078 字节。固定 URL、SHA-256 和上游 MIT 许可副本保存在 `models-license-and-fixture-provenance.json`、`models-fixture-upstream-LICENSE`。仅用于引擎加载与 ASR 冒烟，不是用户真实录音，也不够满足 V10 的 ≥12 条授权多语样本要求。

## 仍需的产品验收

Whisper CPU CLI 已真实加载 Base 模型并转写 11 秒 JFK，输出包含预期的 “ask not what your country can do for you”，退出码 0；记录在 `models-whisper-cpu-smoke.json`。该次为独立进程模型冷加载、OS 页缓存可能已热，且与后台编译争抢 CPU，总耗时 45.059 秒；仅作为加载/转写冒烟，不作为 V11 性能证据，也不是 warm worker。LLM 加载及整理质量由后续阶段继续验证。

开发模型准备工具的 9 项验证（语法、完整性拒绝、错误清理、重定向限制等）记录在 `models-tool-verification.json`；不等同于应用 V06 验收。当前下载和哈希验证不表示中英整理质量、语义保真、录音静音门限、模型生命周期或 V06/V10/V11 已通过。硬件可用内存、上下文 KV 及计算缓冲须由应用运行时验证预算；不因模型文件较小而默认启用远程推理。
