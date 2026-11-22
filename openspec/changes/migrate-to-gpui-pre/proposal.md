# Proposal

## Why

项目现在钉在 `gpui 0.2.2`（crates.io 发布版）上，界面全部由手写的 `div`/`canvas` 原语拼成，没有组件库。想用 `gpui-component 0.9.1`（GPUI Kit 的组件库，60+ 桌面控件）就必须整体迁移：它依赖 `gpui-pre 0.3.12`，与 `0.2.2` 是互不兼容的两个 crate。实测两者并存时 cargo 会同时编译两套 gpui（约 750 个包），且 `Entity`/`Window`/`RenderImage` 类型不同，组件无法嵌入现有视图树。

## What Changes

- 依赖从 `gpui = "=0.2.2"` 切换到 `gpui = { version = "=0.3.12", package = "gpui-pre" }`，并重新生成 `Cargo.lock`。
- 适配 `gpui-pre` 的 API 变化：`ScrollWheelEvent` 更名为 `ScrollEvent`（实测 4 处）。
- 对齐 `windows` crate 版本：`gpui-pre` 引入 `windows 0.58`，而 `ole_drag.rs` 使用 `0.61`，`IDataObject_Impl`/`STGMEDIUM` 的绑定位置不同，需按 0.58 改写拖出模块。
- 迁移完成后引入 `gpui-component`，用它替换手写的右键菜单与 Header 控件。
- 全部既有能力（OSR 渲染、输入转发、IME、尺寸/DPI、共享纹理、多实例、拖放）在迁移后必须逐项验证，不得回退。

## Capabilities

### New Capabilities

- `gpui-platform-baseline`: 定义工程对 gpui 版本的基线约束——单一 gpui 版本、锁定到具体补丁版本、组件库与核心库版本必须自洽，以及迁移后必须保有的可观察行为清单。

### Modified Capabilities

（无。项目此前没有已归档 spec。）

## Impact

- 依赖：`gpui` → `gpui-pre 0.3.12`，新增 `gpui-component 0.9.1` 及其传递依赖；`Cargo.lock` 全量重生成。
- 代码：`src/ui.rs`（输入事件名）、`src/browser/ole_drag.rs`（`windows` 绑定）、`src/windows.rs`（应用启动 API）、`src/browser/render_handler.rs`（渲染/IME 回调签名可能变化）。
- 验证：`scripts/check.ps1` 的全套自测、`scripts/resize-test.py`、`scripts/os-input-test.py`、`scripts/drag-quality-test.py` 需要在新基线上重跑。
- 风险：`gpui-pre` 是预发布线，API 会继续变动；`gpui-component` 精确锁定 `=0.3.12`，升级必须等其跟进。
