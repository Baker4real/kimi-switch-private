---
type: adr
title: GUI 改为 Tauri 纸感壳，视觉语言对齐 Codex Switch
status: accepted
tags: [gui, tauri, visual]
intent: Record why the GUI is a Tauri paper shell aligned with Codex Switch, not egui.
---
# GUI 改为 Tauri 纸感壳，视觉语言对齐 Codex Switch

> **现役外观（as of 2026-09-10）**：#131 起 token 与字体已换成浅蓝白卡片 + IBM Plex Sans / Noto Sans SC 无衬线（`src-ui/styles.test.ts`「浅蓝白卡片 token 体系」、AGENTS.md `src-ui/` 行）。本 ADR 仍约束：Tauri + Vite、两路由（总览 / 账号库）、无深色模式、#119 起无设置页、凭证不下发 webview。暖纸色 / CAS 纸感是换壳当时的外观，不再当施工口径。账号库布局补齐（#137）不重开本 ADR。

kimi-switch 的图形界面从 egui 换成仓库根上的 Tauri + Vite；Rust 命令仍在 `crates/gui`，只把展示用 DTO 交给 webview，凭证不下发。版面是两路由（总览 / 账号库），不提供深色切换（issue #119 起设置页与主题命令一并移除），额度条外形跟 CAS、颜色仍按用量压力跳。换壳时外观曾锁暖纸色以对齐 Codex Switch 的字体、圆角和胶囊壳；继续留在 egui 只能近似，而搬五套主题、Guide、过滤分页则超出本产品已有能力。
