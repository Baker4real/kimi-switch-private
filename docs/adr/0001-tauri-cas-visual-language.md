---
type: adr
title: GUI 改为 Tauri 纸感壳，视觉语言对齐 Codex Switch
status: accepted
tags: [gui, tauri, visual]
intent: Record why the GUI is a Tauri paper shell aligned with Codex Switch, not egui.
---
# GUI 改为 Tauri 纸感壳，视觉语言对齐 Codex Switch

kimi-switch 的图形界面从 egui 换成仓库根上的 Tauri + Vite；Rust 命令仍在 `crates/gui`，只把展示用 DTO 交给 webview，凭证不下发。版面是三路由（总览 / 账号库 / 设置），默认 classic 暖纸色、深色只用 jade-night，额度条外形跟 CAS、颜色仍按用量压力跳。这样做能真用上 CAS 的字体、圆角和胶囊壳；继续留在 egui 只能近似，而搬五套主题、Guide、过滤分页则超出本产品已有能力。
