# 本地 Arch 打包与发布范围

打包入口只消费已验证的本机产物，不执行 Cargo、不重新取网络源码、不安装包或依赖、不修改系统/用户桌面配置。它生成可供用户安装的 Arch 包；生成文件本身不代表通过 V1 全部验收，也不执行对外发布。

## 两种明确模式

- `--development`：验证 JSON 必须为 `status: "development"`、`scope: "linux_development_binary_verification"`，并有非空 `limitations`。包版本在应用版本后加 `devYYYYMMDD`，菜单也标记开发验证版。实际 `vercmp 0.1.0dev20261106 0.1.0` 返回 -1；不要使用会排序更高的 `0.1.0.dev...`。
- 不带该参数：必须为 `status: "passed"`、`scope: "v1_release_acceptance"`，且 V01–V11 每项明确 passed 并绑定实际证据。未完成的跨平台、人工、锁屏后正向交付或真实服务测试不能用开发记录替代。

包版本日期与输出目录时间戳使用工具记录的 UTC 时间。Cargo 和 Tauri 的应用版本必须一致；开发包的外部版本后缀不篡改应用内部版本。

## 先完成实际构建与验证

由主开发流程在目标机完成主程序、CLI 和所选 worker 的构建及相应检查。默认 portable 包需要前四个文件；显式 `--with-avx2` 的本机优化包需要全部六个：

```text
target/release/voice-input
target/release/voice-inputctl
src-tauri/binaries/voice-asr-worker-x86_64-unknown-linux-gnu
src-tauri/binaries/voice-llm-worker-x86_64-unknown-linux-gnu
src-tauri/binaries/voice-asr-worker-avx2-x86_64-unknown-linux-gnu（仅 --with-avx2）
src-tauri/binaries/voice-llm-worker-avx2-x86_64-unknown-linux-gnu（仅 --with-avx2）
```

不要把 debug 程序重命名为 release 程序来满足文件检查，也不要仅因存在文件就填写 verified。实际 workspace 测试、静态检查、worker 模型链路、应用启动及 CLI 等记录应由执行者确认。已知 portable 推理过慢、部分人工桌面测试未做等限制必须保留；开发包不能声称 V1 发行通过。

## 验证 JSON

`schema_version` 为 1。`artifacts` 的键必须恰好为所选的四个或六个路径。每项结构如下（这是结构示意，示意 SHA 不会通过工具验证）：

```json
{
  "sha256": "实际二进制的64位小写SHA256",
  "verified": true,
  "evidence": [
    {"path": "docs/evidence/实际记录.json", "sha256": "该记录文件的实际64位SHA256"}
  ]
}
```

完整顶层包含：

```json
{
  "schema_version": 1,
  "status": "development",
  "scope": "linux_development_binary_verification",
  "limitations": ["填写实际尚未验收或不支持的事项"],
  "artifacts": {},
  "checks": [
    {"name": "填写实际执行的检查名", "status": "passed", "evidence": []}
  ]
}
```

空的 artifacts/evidence 只是示意，不是可打包输入。工具要求实际证据文件存在、非空、SHA 匹配，所有输入均为可执行 x86-64 Linux ELF，且复制后的字节仍与验证 SHA 一致。路径均为仓库相对路径。检查中有失败/未测项时，不得将其写成 passed；如它是开发包允许的剩余限制，应在 limitations 中说明，不作为虚假的已通过检查。

稳定模式另外要求 `v1_acceptance` 恰好有 V01–V11，各项为 `{"status":"passed","evidence":[{"path":"...","sha256":"..."}]}`。这仍依赖执行者诚实提供真实证据，工具不会把模拟、编译成功或日志文件存在自动解释为人工验收。

## 准备或构建

```bash
python3 scripts/package-arch.py --development \
  --validation docs/evidence/release-validation.json --prepare-only

python3 scripts/package-arch.py --development \
  --validation docs/evidence/release-validation.json

# 参考机同时安装已验证优化档时，JSON 必须覆盖六个文件
python3 scripts/package-arch.py --development --with-avx2 \
  --validation docs/evidence/release-validation.json
```

两次运行默认使用新的 UTC 时间戳输出目录；若指定 `--output-dir`，目录必须尚不存在，避免覆盖旧证据。`--prepare-only` 只生成 payload/PKGBUILD，不运行 makepkg。第二条会运行普通用户的 `makepkg --cleanbuild --noconfirm`，不使用 `-s`、`--install`、sudo 或未固定的网络 source。实际依赖不足或二进制/证据缺失会停止。

PKGBUILD 使用本地 `payload.tar` 及完整 SHA-256，显式禁止 strip/debug 重写，保留已验证二进制字节。完成后逐个读取归档内所选程序并重新比对 SHA；禁止混入 `.gguf`、`.bin` 权重或用户 SQLite 数据库。包内带模型目录来源与许可，不带模型权重，不自动下载。旧 CPU 构建默认只用 portable；不要为了打包而在不具备特征的机器强行构建/执行 AVX2 档。

输出含：

- `release-manifest.json`：验证范围、限制、运行库实际观察、所有 payload 文件 SHA/大小/模式；自校验文件与 ALPM 自动元数据单独说明排除。
- `archive-manifest.json`：只有 makepkg 实际成功、归档内容及所选程序字节检查通过后才生成，记录真实包文件 SHA/大小。`installed` 和 `uninstall_tested` 仍为 false。
- `PKGBUILD`、`payload.tar`、实际包及 `makepkg.log`，便于复核构建步骤。

安装/卸载需按 `INSTALL.md` 在用户正常图形会话另做实测。脚本不会把归档检查伪称为安装或卸载通过。

## 第三方来源与许可元数据

```bash
python3 scripts/package-arch.py --metadata-only --output-dir dist/license-scan-new
```

元数据工具不运行 Cargo、不访问网络。它读取 Cargo.lock 和本机 Cargo 缓存，优先核对 `.crate` 归档 SHA 与锁清单，再比对缓存 Cargo.toml；兼容旧缓存的 checksum 清单。记录依赖名称、版本、许可表达式/许可文件、来源、校验值和缺失状态。包括仅其他目标平台使用或未实际链接的锁定依赖，因此不是精确链接 SBOM。

开发包会明确保留 incomplete_count；稳定包在元数据仍不完整时拒绝构建。包附项目、两引擎与 nlohmann JSON 的原始许可全文，目录模型的原始许可及来源映射。Rust 依赖清单和这些副本不等于第三方全部完成法律审核，清单固定标记 `legal_review_status: "not_performed"`。

源码快照记录打包时观察到的文件 SHA 和 Git 版本，不把有未提交工作的快照声称为按干净提交逐字节可重现的构建。需要发布时应由实际发布流程保留源代码、锁清单、工具链、证据与最终归档哈希。

## 工具本身的验证范围

`packaging/arch/tool-verification.json` 保留 23 项自有临时 fixture 的结构与拒绝门检查：仅检验四/六输入的结构、SHA/证据变化拒绝、开发/稳定模式、缺失 V09 拒绝、symlink 拒绝、payload 安装布局、本地 source checksum、模式与渲染后的 Bash 语法。fixture 使用临时复制的占位 ELF，并非应用运行证据，未执行 Cargo、makepkg 或安装。

实际本机缓存的 Cargo 元数据扫描覆盖 601 个锁定包，15 项 Windows/macOS 专有缓存缺失，均明确标记；没有据此伪造许可或安装结果。Desktop 文件验证、Lua/Bash/Python 语法检查通过。实际应用包是否已构建、安装或卸载，必须另以真实 `archive-manifest.json` 与安装验收记录判断，不能引用这些结构测试替代。
