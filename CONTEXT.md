# kimi-switch

本机 Kimi Code 多账号库：把多个官方登录身份收进本地，手动把其中一个写成正在使用的凭证。

## Language

**账号**:
Kimi Code 的一个官方登录身份在本机账号库里的副本，用账号 id 区分，可用别名显示。
_Avoid_: profile, 配置, 用户

**账号库**:
本机收纳的全部账号这一集合。
_Avoid_: Profiles, 配置目录

**当前账号**:
账号库里此刻对应 Kimi Code 正在使用的那份登录凭证的账号。
_Avoid_: active profile, 当前配置

**停泊账号**:
账号库里此刻不对应 Kimi Code 当前登录文件的账号。
_Avoid_: parked（仅代码内部名）, 非当前账号, 闲置账号

**别名**:
用户给账号起的显示名，只影响本软件里怎么称呼它，不改官方身份。
_Avoid_: 昵称, folder name

**额度**:
某个账号在 Kimi 官方用量窗口（5 小时 / 7 天等）里已经用掉或还剩多少。
_Avoid_: metric, quota monitor

**额度查询**:
向 Kimi 官方用量接口询问某个账号此刻的额度。
_Avoid_: 额度刷新（易和 token 轮换、界面「刷新」按钮混在一起）

**token 轮换**:
官方登录凭证的一次性更换机制：refresh token 用过即废，官方客户端续期后把新凭证写回 Kimi Code 当前登录文件。
_Avoid_: token 刷新（说机制时用轮换，避免和界面「刷新」混淆）

**额度告急**:
某个账号任一用量窗口已用达到或超过 80%，或该账号额度查询失败。
_Avoid_: low quota, exhausted（exhausted 只表示用尽）

**切换**:
把某一个库存账号的凭证写成 Kimi Code 当前登录文件，使它成为当前账号。
_Avoid_: swap（仅 CLI 内部子命令）, activate profile

**导入**:
把本机 Kimi Code 此刻已登录的身份收进账号库；不代替切换。
_Avoid_: 同步, capture live

**设备码授权**:
用浏览器打开官方设备码链接、完成登录后，把新身份收进账号库。
_Avoid_: OAuth 登录（对用户）, device login

**重新授权**:
对一张已有的账号卡重走设备码授权，用同一官方身份的新凭证覆盖这张卡；官方身份对不上会被拒绝，别名保留、不新增卡。
_Avoid_: 续命, 重新登录, 换绑

**会员等级**:
官方返回的订阅档位展示名（如 Intermediate、Advanced），只用于辨认账号。
_Avoid_: 套餐, plan
