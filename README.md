# kimi-switch · Kimi 换号器

[Kimi Code](https://www.kimi.com/) 多账号管理工具：浏览器授权添加账号、一键切换、额度一目了然。
提供 Windows 图形界面和命令行两个版本，单文件 exe、无运行时依赖。

![深色主题](docs/screenshot-dark.png)

![浅色主题](docs/screenshot-light.png)

## 功能

- **浏览器授权添加账号**：点「＋ 添加账号」获取授权链接，在浏览器登录授权后自动入库（设备码授权），全程不影响当前正在使用的账号。
- **按卡重新授权**：账号过期不用删除重加——点卡片上的「授权」对这一张卡重走设备码授权；官方身份对不上会被拒绝，凭证不会写错卡。
- **一键切换账号**：点「切换」原子替换 `~/.kimi-code/credentials/kimi-code.json`，**无需退出重登、无需重启**，立即生效；每次切换自动留快照，失败可回滚。
- **导入当前账号**：把本机 Kimi Code 已登录的账号一键收进账号库。
- **额度可视化**：5 小时 / 7 天双窗口彩色进度条（绿 → 黄 → 橙 → 红），条上直接标注百分比与用量，右侧显示窗口重置时间，附带官网会员档位徽章（Allegretto、Allegro 等）。窗口可见时静默查询；点「刷新」与 CLI 共用 90 秒节流和失败退避，陈旧回退时卡片标注缓存时间。
- **凭证待续期引导**：当前账号 access token 过期导致的 401 单独标成「凭证待续期」——不算额度告急、不按错误红渲染，并提示恢复路径（在 Kimi CLI 里发条消息，或对这张卡重新授权）。
- **当前账号主动续期（可选）**：设置页开关，默认关闭、仅 Windows 生效；开启后在查询额度前为当前账号原子轮换登录凭证，不等官方客户端的懒刷新；极端情况下官方会话可能要求重新登录。
- **账号管理**：重命名别名、删除（带确认弹窗）。
- **纸色 / 深色主题**：默认暖纸色，设置里可换成 jade-night 深色。
- **凭证安全**：token 只保存在本机私有目录（Unix 下 0600 权限），网络请求只发往 Kimi 官方域名（`auth.kimi.com` / `api.kimi.com`），零遥测；token 刷新在有官方锁协议的平台（macOS/Linux）与官方客户端用同一套锁协调；Windows 上官方无锁协议，默认不替当前账号续期、不会互相抢刷（可选「主动续期」见上文）。

## 安装

### 下载成品

从 [Releases](../../releases) 下载 `kimi-switch.exe`（图形界面）或 `kimi-switch-cli.exe`（命令行），双击即用。

### 从源码构建

需要 Rust 1.80+ 和 Node.js（图形界面）：

```bash
git clone https://github.com/Baker4real/kimi-switch-private.git
cd kimi-switch
cargo build --release -p kimi-switch-cli
npm install
npm run tauri build
```

产物：`target/release/kimi-switch.exe`（图形界面）、`target/release/kimi-switch-cli.exe`（命令行）、`target/release/bundle/`（MSI/NSIS 安装包）。

## 使用

### 图形界面

双击 `kimi-switch.exe`：

| 操作 | 说明 |
|---|---|
| ＋ 添加账号 | 弹出授权链接 → 浏览器登录授权 → 自动入库 |
| 导入当前账号 | 把 Kimi Code 当前登录的账号存入账号库 |
| 切换 | 把该账号写入本地凭证，立即生效 |
| 授权 | 给这一张卡重走浏览器设备码授权，救活过期账号（当前账号卡上没有「切换」，只有「授权」） |
| 重命名 | 给账号起别名（账号 id 是一串随机字符，别名方便辨认） |
| 删除 | 从账号库移除（不影响 Kimi Code 当前登录文件） |
| 刷新 | 按额度快照再查一遍（90 秒节流内不打官方接口；退避中显示上次成功数据） |

### 命令行

```bash
kimi-switch-cli                 # 账号列表 + 5h/7d 额度（带缓存节流与失败退避，不会高频打接口）
kimi-switch-cli login kimi      # 导入当前 Kimi Code 已登录账号
kimi-switch-cli swap <编号或id>  # 切换激活账号
kimi-switch-cli rm <编号或id>    # 删除账号
kimi-switch-cli config           # 打印生效设置（只读，与 GUI 设置页同源）
```

GUI 与 CLI 共用同一份账号库和设置（`%APPDATA%\kimi-switch\`），可以混用。

## 常见问题

**切换后需要重启 Kimi Code 吗？**
不需要。凭证是每次请求时读取的，切换即时生效。

**账号库的凭证安全吗？**
所有 token 只存在你自己的电脑上（`%APPDATA%\kimi-switch\` 下的私有文件），不会上传到任何第三方服务器。源码中唯一的网络请求是 Kimi 官方的授权与额度接口。

**账号放久了切换过去提示 401？**
refresh token 有有效期，长期未使用的账号凭证会过期。在账号库对那张卡点「授权」重走一次浏览器登录即可救活，无需删除重加。

**当前账号额度查询提示 401，其他账号却正常？**
当前账号的额度查询直接使用 Kimi Code 当前登录文件里的 access token，它只有约 15 分钟有效期，官方客户端闲置时不会自动续期；本工具默认不替当前账号续期，避免与官方客户端争抢凭证。这种情况会单独标成「凭证待续期」：在 Kimi CLI 里发一条消息（触发官方续期）后重新查询即可恢复；若仍反复 401，再对这张卡点「授权」重走登录。Windows 用户也可以在设置里打开「当前账号主动续期」，让工具在查询前自动轮换凭证。

## 免责声明

本工具为个人开源项目，与 Moonshot AI / Kimi 官方无关。多账号使用方式可能涉及官方服务条款的灰色地带，请自行评估风险，账号安全与使用后果自负。本项目仅供学习交流。

## License

[MIT](LICENSE)
