# 源码、模型与依赖冻结状态

状态：**未冻结，不可发布**。只有取得源码和权重、核验并成功构建/加载后才生成正式锁定清单。当前不存在可信完整模型 SHA-256 或成功构建的源码 commit，不能用分支名、网页摘要或 40 位 SHA 替代。

## 引擎

| 引擎 | 官方来源 | 许可信息 | 固定提交 / CPU 构建 |
|---|---|---|---|
| whisper.cpp | https://github.com/ggml-org/whisper.cpp | 上游页面标注 MIT，打包时核对固定提交 LICENSE | 待取得 / 未执行 |
| llama.cpp | https://github.com/ggml-org/llama.cpp | 上游页面标注 MIT，打包时核对固定提交 LICENSE | 待取得 / 未执行 |

两个源仓库的 Shell Git 查询均失败：无法解析 github.com；证据见 M0 JSON。文档浏览工具能读取公开说明，不代表 Shell 具备下载、依赖解析与构建能力。两个 worker 必须分开链接各自引擎，CPU 是发布基线。

## 首批候选模型

| 用途 | 候选 | 来源 / 许可信息 | 尚缺验证 |
|---|---|---|---|
| ASR | Whisper Base 多语 `ggml-base.bin` | [whisper.cpp 转换模型库](https://huggingface.co/ggerganov/whisper.cpp)，模型卡标注 MIT | 固定仓库 revision、单文件大小、完整 SHA-256、权重许可文件、CPU 加载及中英转写 |
| 本地 LLM | Qwen2.5-1.5B-Instruct-GGUF，Q4_K_M | [Qwen 官方模型库](https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF)，模型卡标注 Apache-2.0 | 核对精确单文件及大小、固定 revision、完整 SHA-256、许可/NOTICE、上下文与 CPU 内存实测、中英整理质量 |

候选只是 M0 的起点，未下载、激活或作为可发布默认。当前可用内存仅约 4 GiB，需先验证预算和实际加载；不自动切换远程服务。ASR 模型页的 SHA 列是 40 位摘要，不能当成规格要求的 SHA-256。

## 依赖

M0 JSON 已保存实际 PATH、运行版本和本机软件包快照。这是环境证据，不等于应用依赖锁。M1 需固定 Rust toolchain、Cargo.lock、前端 lockfile、CMake 选项和两套引擎提交。目前未创建无实际解析/构建支撑的锁文件。
