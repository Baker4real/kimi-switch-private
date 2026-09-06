---
type: adr
title: Windows 任务栏图标补设 ICON_BIG
status: accepted
tags: [taskbar-icon, windows, tauri, wm-seticon]
intent: Record why kimi-switch must self-set ICON_BIG/ICON_SMALL after startup (tao only sets SMALL), the 1s-delay refresh contract with the Win11 taskbar, and the DWM PrintWindow verification path.
documents:
  symbols:
    - fix_taskbar_icon
---
# Windows 任务栏图标补设 ICON_BIG

2026-09-06 用户报告：任务栏按钮显示白色纸张状占位图，不是应用图标（仓库 icon.ico 全部
尺寸层经像素级核对均为墨绿 K，无坏层）。本 ADR 钉死根因链、修复时机契约与验证通道，
防止后续「简化」掉延迟或图标设置时看不见地回归。

## 决策

1. **根因是上游缺口，不是图标资产问题。**
   tao 0.35.3（Tauri 2.11 全系）在 Windows 上 `set_window_icon` 只 `WM_SETICON`
   ICON_SMALL（16px），`set_taskbar_icon`（ICON_BIG 槽）Tauri 未接线、从不设置；
   窗口类注册时 `hIcon` 亦为空。Win11 任务栏按钮在 >100% 缩放下读 ICON_BIG 槽，
   为空时渲染白色占位图，且不会回退到 SMALL 槽或 exe 图标。alt-tab 缩略图同取
   BIG 槽，受同一缺口影响。

2. **修复必须延迟约 1 秒，不能在 setup 里立即设置。**
   实测 explorer 在按钮建立时读一次图标，启动瞬间（窗口创建后、setup 内）设置赶不上
   首读；而「保持 BIG 为空、之后再设」的非空跃迁会触发任务栏刷新，等值换句柄不会。
   因此 `fix_taskbar_icon` 在 setup 里仅调度：独立线程睡 1 秒后统一补设两槽。把延迟
   去掉或把设置提前都会让按钮停在占位图，且现有测试链完全探测不到。

3. **图标源是 exe 自带资源（tauri-winres 嵌入，资源 ID 32512），尺寸按窗口 DPI。**
   `LoadImageW(hModule, 32512, IMAGE_ICON, cx, cy)` 从图标组里自动选最佳层，
   与安装后 exe 图标同源，无第二份真相。尺寸取 32/16 逻辑像素 × `scale_factor`
   （150% 下即 48/24）。HICON 不转移所有权给窗口，须存活到进程结束，代码里有意
   不释放。失败只打 stderr，不阻断启动——任务栏图标是展示性问题，不该挡住换号。

4. **验证通道用 DWM，不用屏幕截取。**
   任务栏渲染无法进单测（explorer 行为）。端到端验证用
   `PrintWindow(Shell_TrayWnd, PW_RENDERFULLCONTENT)` 让 DWM 把任务栏渲染进位图
   后按 K 图标配色统计像素（显示器休眠时 `CopyFromScreen` 全黑，DWM 通道不受
   显示电源影响）。排查时另需注意 `WM_GETICON = 0x7F`、`WM_SETICON = 0x80`，
   常量写反是纯 no-op 且无报错。
