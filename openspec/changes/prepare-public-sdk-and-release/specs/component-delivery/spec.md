# component-delivery Specification Delta

## ADDED Requirements

### Requirement: Public cef-rs-gpui branding and zero-mutation workspace hygiene
仓库顶层说明文档与根清单元数据 SHALL 与 `cef-rs-gpui` 公开品牌保持一致，同时消除 `gpui-component` 等工作区未使用依赖编译告警并完成 `legacy-demo` 遗留形态的清理/归档评估；上述整理 SHALL 保持全部 4 份 `Cargo.lock` 字节不变、`crates/rust-cef-core` 零修改以及 `scripts/check.ps1` 与 `scripts/test-component-demo.ps1` 既有验证契约可用。

#### Scenario: Workspace builds without unused dependency warnings or lockfile drift
- **WHEN** 开发者或 CI 在工作区根目录执行离线锁定检查（`cargo check --locked`）
- **THEN** 构建不再产生 `unused_workspace_dependencies`（`gpui-component`）警告，`README.md` 与 `Cargo.toml` 准确体现 `cef-rs-gpui` 身份，且全部 4 份 `Cargo.lock` 与 `crates/rust-cef-core` 字节级保持不变

#### Scenario: Legacy demo assessment preserves existing check and validation scripts
- **WHEN** 完成 `legacy-demo`（`src/main.rs`、`src/windows.rs`、`src/ui.rs`、`src/browser/`）依赖图与维护边界审计并执行既有验证脚本
- **THEN** 文档与配置明确标识 `legacy-demo` 的非默认遗留/归档状态，`scripts/check.ps1`、`scripts/test-component-demo.ps1` 与 `runtime-probe` 全部现有契约继续完整运行并通过

### Requirement: Automated CI verification and reproducible Windows release packaging
仓库 SHALL 提供自动化 CI 工作流与 Windows x64 发行工作流：CI 覆盖代码格式检查、跨平台核心与 UI 契约测试及 Windows 宿主编译检查；Release 工作流支持标签或手动触发，复用仓库打包工具输出独立 Windows x64 发行压缩包并附带经校验的 SHA-256 清单文件。

#### Scenario: CI workflow validates formatting, cross-platform crates, and Windows checks
- **WHEN** 代码推送或 Pull Request 触发 `.github/workflows/ci.yml`
- **THEN** 流水线执行 `cargo fmt --check`、跨平台 `rust-cef-core` / `rust-cef-ui` 检查与测试，并在 Windows x64 runner 上使用短构建路径与 UTF-8 工具链运行 `scripts/check.ps1` 及工作区检查

#### Scenario: Release workflow packages Windows x64 bundle with SHA-256 manifest
- **WHEN** 发布标签或手动触发 `.github/workflows/release.yml`
- **THEN** 工作流在 Windows x64 MSVC 环境构建 Release 产物并调用 `scripts/package-component.ps1` 打包离线发行目录，生成包含逐文件校验和的 SHA-256 清单与压缩包作为可下载发行制品
