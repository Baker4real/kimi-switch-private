//! 绑定网页登录（issue #50，ADR-0004）：用网页消费者 token 读取会员到期；
//! 以及会员到期的「仅刷新」路径（issue #52）。
//!
//! 会员到期日只存在于网页消费者接口 `www.kimi.com` 的 `GetSubscription`
//! （Connect RPC），`/usages` 与 Code OAuth 都没有这份数据。用户从网页端复制
//! token 粘贴给 CLI（stdin 读取，TTY 隐藏回显），本模块完成绑定管道：
//!
//! 1. 身份门（一切写入与网络之前）：解码网页 JWT payload，`sub`（回退
//!    `user_id`）必须等于目标账号 id——与按卡重新授权同型的门（ADR-0002）；
//!    对不上或无法辨认 → 英文错误、不写网页 field、不写 extra、不打网络；
//! 2. 网页凭证存独立 field [`WEB_STORE_FIELD`]（不碰 Code blob 的
//!    store_field，同账号两条互不覆盖）。会员读取失败不回滚这一步：身份已
//!    验证的 token 是有效绑定，到期日期可由重试 / 后续刷新（#52）补齐；
//! 3. 请求一次 `GetSubscription`；
//! 4. 只读 `subscription.currentEndTime` / `subscription.nextBillingTime`，
//!    **忽略 `balances[].expireTime` 与一切月度周期字段**——它们是额度余额
//!    的有效期，不是会员到期（ADR-0004）；`goods.title` / `membershipLevel`
//!    仅核对，不落库、不覆盖 `/usages` 档位展示；
//! 5. 按 #49 schema 写 extra：读出 Account → 只动三个 membership 键 →
//!    registry upsert（保留 dedup / scope 等既有键，绝不整体重建）；
//! 6. 返回展示用到期日（`format_membership_expiry`，东八区日历日）。
//!
//! 网页会员请求的完整触发口径（#52，根 Agent 裁定）：只发生在两处——
//! 绑定（[`bind_web`]，#50）与用户发起刷新（GUI 行列组装的
//! `QuotaCollectReason::UserInitiated` 路径对已绑定账号调用
//! [`refresh_web_membership`]，缓存日历日无论新旧、含已过期都请求）。
//! Scheduled / 静默额度收集 / 启动加载 / 操作（swap / import / 绑定等）
//! 后再列一律 0 次网页请求。
//!
//! 取消续费但 active 的订阅照常返回到期日，不产生「已取消」语义。
//! 请求固定发往 `www.kimi.com`（常量，无环境变量覆写），UA 用 `kimi-switch`；
//! 错误文案一律英文且不含 token。本模块不被 `swap` / `activate` / 额度查询
//! 路径调用（网页接口只属于绑定与用户发起的刷新）。

use std::time::Duration;

use chrono::{DateTime, Utc};
use kimi_switch_core::error::{Error, Result};
use kimi_switch_core::membership_expiry::{
    format_membership_expiry, membership_period_days, write_membership_extra,
};
use kimi_switch_core::{AccountId, AccountRegistry, CredentialStore, WEB_STORE_FIELD};

use crate::PROVIDER_ID;

/// 网页会员接口固定域名（ADR-0004：网络白名单成员之一，不做环境变量覆写）。
pub const MEMBERSHIP_BASE_URL: &str = "https://www.kimi.com";
/// `GetSubscription` 的 Connect RPC 路径。
const GET_SUBSCRIPTION_PATH: &str =
    "/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscription";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// 绑定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebBinding {
    /// 展示用到期日（东八区 `YYYY-MM-DD`，如 `2027-08-27`）；订阅无到期
    /// 数据时 `None`（展示层回退绑定弱提示）。
    pub expiry_text: Option<String>,
    /// 当前会员周期天数（到期展示日 − `currentStartTime` 的东八区日历日）；
    /// 开始瞬间或到期数据缺失时 `None`（会员轨道不画填充）。
    pub period_days: Option<i64>,
}

/// 由开始瞬间与到期展示日折算周期天数；任一缺失返回 `None`。
fn binding_period_days(
    current_start: Option<DateTime<Utc>>,
    expiry_text: &Option<String>,
) -> Option<i64> {
    current_start
        .zip(expiry_text.as_deref())
        .and_then(|(start, day)| membership_period_days(start, day))
        .map(i64::from)
}

/// 对目标账号绑定网页登录（生产入口，固定 [`MEMBERSHIP_BASE_URL`]）。
///
/// `web_token` 由调用方从 stdin 读入（TTY 隐藏回显）；本函数不读 stdin，
/// 便于 GUI（#51）与测试复用。
pub async fn bind_web(
    registry: &AccountRegistry,
    store: &dyn CredentialStore,
    target: &AccountId,
    web_token: &str,
) -> Result<WebBinding> {
    bind_web_at(registry, store, target, web_token, MEMBERSHIP_BASE_URL).await
}

/// 注入式绑定管道：`pub(crate)`——仅供同 crate 测试注入 `base_url`，
/// 生产一律走 [`bind_web`]。
pub(crate) async fn bind_web_at(
    registry: &AccountRegistry,
    store: &dyn CredentialStore,
    target: &AccountId,
    web_token: &str,
    base_url: &str,
) -> Result<WebBinding> {
    // 目标账号必须存在：extra 要写回它的 registry 行。
    let account = registry.find(PROVIDER_ID, target)?.ok_or_else(|| {
        Error::AccountNotFound {
            provider: PROVIDER_ID.into(),
            id: target.to_string(),
        }
    })?;

    // 身份门在一切写入与网络之前（与按卡重新授权同型，见 ADR-0002/0004）。
    let observed = web_identity(web_token).ok_or_else(Error::bind_web_unidentifiable)?;
    if observed != account.id.0 {
        return Err(Error::bind_web_identity_mismatch(observed, &account.id.0));
    }

    // 网页凭证独立 field（与 Code blob 的 store_field 分离，见模块 doc 第 2 点）。
    store.set(
        PROVIDER_ID,
        account.id.0.as_str(),
        WEB_STORE_FIELD,
        web_token,
    )?;

    let payload = get_subscription_at(base_url, web_token).await?;

    // 只读三个会员瞬间；balances[].expireTime 等其余字段一律忽略（ADR-0004）。
    let current_start = read_instant(&payload, "/subscription/currentStartTime");
    let current_end = read_instant(&payload, "/subscription/currentEndTime");
    let next_billing = read_instant(&payload, "/subscription/nextBillingTime");

    // 写 extra：网络往返后重读 registry 行再改写，缩小并发窗口；行被并发删除
    // 则清掉刚写的网页 field 再报账号不存在（不留孤儿凭证）。
    let mut fresh = match registry.find(PROVIDER_ID, target)? {
        Some(account) => account,
        None => {
            let _ = store.delete(PROVIDER_ID, target.0.as_str(), WEB_STORE_FIELD);
            return Err(Error::AccountNotFound {
                provider: PROVIDER_ID.into(),
                id: target.to_string(),
            });
        }
    };
    write_membership_extra(&mut fresh.extra, current_start, current_end, next_billing);
    registry.upsert(fresh)?;

    let expiry_text = format_membership_expiry(current_end, next_billing);
    let period_days = binding_period_days(current_start, &expiry_text);
    Ok(WebBinding {
        expiry_text,
        period_days,
    })
}

/// 仅刷新已绑定账号的会员到期（issue #52，生产入口，固定 [`MEMBERSHIP_BASE_URL`]）。
///
/// 触发口径（#52）：只在用户发起刷新（`QuotaCollectReason::UserInitiated` 的
/// GUI 行列组装路径）对已绑定账号调用；缓存日历日无论新旧（含已过期）都重打
/// `GetSubscription`。Scheduled / 静默收集 / 启动加载 / 操作后再列不得调用。
///
/// 与绑定的差异：网页凭证已由绑定管道写好，这里不再做身份门、不写网页 field，
/// 只用已存 token 重打一次 `GetSubscription`——成功则按 #49 schema 重写 extra
/// 两个键（日期可能更新；订阅已无到期数据时清掉旧键）；任何失败（网络 / HTTP /
/// 解析，含 401 类网页 token 失效）extra 与网页凭证原样不动、返回英文错误
/// （不含 token）。缓存降级语义（行内保留旧日期 + 绑定提示再现）由 GUI 层承接，
/// 本层不删任何已有数据。
pub async fn refresh_web_membership(
    registry: &AccountRegistry,
    store: &dyn CredentialStore,
    target: &AccountId,
) -> Result<WebBinding> {
    refresh_web_membership_at(registry, store, target, MEMBERSHIP_BASE_URL).await
}

/// 注入式仅刷新管道：`pub(crate)`——仅供同 crate 测试注入 `base_url`，
/// 生产一律走 [`refresh_web_membership`]。
pub(crate) async fn refresh_web_membership_at(
    registry: &AccountRegistry,
    store: &dyn CredentialStore,
    target: &AccountId,
    base_url: &str,
) -> Result<WebBinding> {
    // 目标账号必须存在：extra 要写回它的 registry 行。
    let account = registry.find(PROVIDER_ID, target)?.ok_or_else(|| {
        Error::AccountNotFound {
            provider: PROVIDER_ID.into(),
            id: target.to_string(),
        }
    })?;
    // 只有已绑定（凭证仓库里有网页 field）才可刷新；未绑定说明调用方越过了
    // 触发口径，英文错误、不打网络。
    let web_token = store
        .get(PROVIDER_ID, account.id.0.as_str(), WEB_STORE_FIELD)?
        .ok_or_else(|| {
            Error::provider("membership refresh requires an existing web binding")
        })?;

    let payload = get_subscription_at(base_url, &web_token).await?;
    // 只读三个会员瞬间（口径与绑定一致，ADR-0004）。
    let current_start = read_instant(&payload, "/subscription/currentStartTime");
    let current_end = read_instant(&payload, "/subscription/currentEndTime");
    let next_billing = read_instant(&payload, "/subscription/nextBillingTime");

    // 成功才写 extra：网络往返后重读 registry 行再改写（与绑定同一写序）；
    // 行被并发删除按账号不存在失败——网页凭证是绑定管道写的，不在这里清理。
    let mut fresh = registry.find(PROVIDER_ID, target)?.ok_or_else(|| {
        Error::AccountNotFound {
            provider: PROVIDER_ID.into(),
            id: target.to_string(),
        }
    })?;
    write_membership_extra(&mut fresh.extra, current_start, current_end, next_billing);
    registry.upsert(fresh)?;

    let expiry_text = format_membership_expiry(current_end, next_billing);
    let period_days = binding_period_days(current_start, &expiry_text);
    Ok(WebBinding {
        expiry_text,
        period_days,
    })
}

/// 网页 JWT 身份辨认：payload 的 `sub`（回退 `user_id`）。
/// 复用 `kimi_files::decode_jwt_payload`（base64url + JSON），不验签——
/// 只用于本地身份比对，token 真伪由 `GetSubscription` 的 401 兜底（ADR-0004）。
fn web_identity(web_token: &str) -> Option<String> {
    let claims = crate::kimi_files::decode_jwt_payload(web_token)?;
    let id = claims
        .get("sub")
        .and_then(|value| value.as_str())
        .or_else(|| claims.get("user_id").and_then(|value| value.as_str()))?;
    Some(id.to_string())
}

/// 请求 `GetSubscription`（Connect RPC：JSON 编码 + `connect-protocol-version: 1`）。
/// 错误文案英文且不含 token：不回显响应 body（只带状态码），请求错误只带 URL 侧原因。
async fn get_subscription_at(base_url: &str, web_token: &str) -> Result<serde_json::Value> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), GET_SUBSCRIPTION_PATH);
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| Error::provider(format!("membership client failed: {e}")))?;
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("connect-protocol-version", "1")
        .header("User-Agent", "kimi-switch")
        .header("Authorization", format!("Bearer {web_token}"))
        .body("{}")
        .send()
        .await
        .map_err(|e| {
            Error::provider(format!("membership subscription request failed: {e}"))
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::provider_http(
            status.as_u16(),
            format!("membership subscription HTTP {status}"),
        ));
    }
    let body = resp.text().await.unwrap_or_default();
    serde_json::from_str(&body)
        .map_err(|e| Error::provider(format!("membership subscription parse failed: {e}")))
}

/// 读一个 RFC3339 瞬间；字段缺失、非字符串或解析失败一律按 `None`（无该数据）。
fn read_instant(payload: &serde_json::Value, pointer: &str) -> Option<DateTime<Utc>> {
    let raw = payload.pointer(pointer)?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|instant| instant.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{jwt_with_payload, MockServer};
    use chrono::Utc;
    use kimi_switch_core::membership_expiry::{
        membership_expiry_from_extra, EXTRA_CURRENT_END_TIME, EXTRA_CURRENT_START_TIME,
        EXTRA_NEXT_BILLING_TIME,
    };
    use kimi_switch_core::{Account, FileStore};

    /// 官网 `GetSubscription` 夹具（结构对齐 2026-08-31 实抓 + 验收要求的干扰项）：
    /// - `balances[].expireTime` 是额度余额的月度周期，故意放到 2026-09-28 做干扰；
    /// - `SUBSCRIPTION_STATUS_CANCEL` + `active: true`：取消续费但仍在期，
    ///   照常返回到期日，不产生「已取消」语义。
    const SUBSCRIPTION_FIXTURE: &str = r#"{
      "subscription": {
        "subscriptionId": "sub-1",
        "status": "SUBSCRIPTION_STATUS_CANCEL",
        "active": true,
        "membershipLevel": "LEVEL_INTERMEDIATE",
        "currentStartTime": "2026-08-28T00:00:00Z",
        "currentEndTime": "2027-08-28T00:00:00Z",
        "nextBillingTime": "2027-08-27T01:25:37.930341Z",
        "balances": [{"quotaId": "b1", "expireTime": "2026-09-28T00:00:00Z"}]
      },
      "goods": {"goodsId": "g1", "title": "Kimi Membership"}
    }"#;

    /// 只有 `currentEndTime`（UTC 零点）的夹具：验证重新绑定会移除旧的
    /// `nextBillingTime` 键、不残留旧日期。
    const CURRENT_END_ONLY_FIXTURE: &str = r#"{
      "subscription": {
        "status": "SUBSCRIPTION_STATUS_ACTIVE",
        "active": true,
        "currentEndTime": "2028-08-28T00:00:00Z"
      },
      "goods": {"goodsId": "g1", "title": "Kimi Membership"}
    }"#;

    /// 订阅无任何到期瞬间的夹具：刷新成功也要清掉旧日期键（回到「无到期日」）。
    const NO_EXPIRY_FIXTURE: &str = r#"{
      "subscription": {"status": "SUBSCRIPTION_STATUS_ACTIVE", "active": true},
      "goods": {"goodsId": "g1", "title": "Kimi Membership"}
    }"#;

    struct Harness {
        _tmp: tempfile::TempDir,
        registry: AccountRegistry,
        store: FileStore,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let registry = AccountRegistry::new(tmp.path().join("registry.toml"));
            let store = FileStore::new(tmp.path().join("credentials.json"));
            Self {
                _tmp: tmp,
                registry,
                store,
            }
        }

        /// 注册一个带既有 extra 键（dedup/scope，模拟真实账号）与 Code blob 的账号。
        fn seed(&self, id: &str) -> Account {
            let mut extra = serde_json::Map::new();
            extra.insert(
                "dedup_key".into(),
                serde_json::Value::String(format!("stable-{id}")),
            );
            extra.insert(
                "scope".into(),
                serde_json::Value::String("kimi-code".into()),
            );
            let account = Account {
                provider: "kimi".into(),
                id: AccountId(id.into()),
                label: id.into(),
                active: false,
                created_at: Utc::now(),
                last_used_at: None,
                extra,
            };
            self.registry.upsert(account.clone()).unwrap();
            self.store
                .set(
                    "kimi",
                    id,
                    "blob",
                    &format!(r#"{{"uid":"{id}","access_token":"A1"}}"#),
                )
                .unwrap();
            account
        }

        fn bound_account(&self, id: &str) -> Account {
            self.registry
                .find("kimi", &AccountId(id.into()))
                .unwrap()
                .expect("账号应在 registry 中")
        }

        /// 预置「已绑定」账号（#52 刷新路径夹具）：网页凭证已写、extra 带旧
        /// 到期数据（日历日 2026-08-27，已过期）与旧周期开始键，
        /// 等价于绑定后过了一段时间。
        fn seed_bound(&self, id: &str) -> Account {
            self.seed(id);
            let web_token = token_for(id);
            self.store
                .set("kimi", id, WEB_STORE_FIELD, &web_token)
                .unwrap();
            let mut bound = self.bound_account(id);
            bound.extra.insert(
                EXTRA_CURRENT_START_TIME.into(),
                serde_json::Value::String("2025-08-28T00:00:00Z".into()),
            );
            bound.extra.insert(
                EXTRA_CURRENT_END_TIME.into(),
                serde_json::Value::String("2026-08-28T00:00:00Z".into()),
            );
            bound.extra.insert(
                EXTRA_NEXT_BILLING_TIME.into(),
                serde_json::Value::String("2026-08-27T01:25:37.930341Z".into()),
            );
            self.registry.upsert(bound.clone()).unwrap();
            bound
        }
    }

    fn token_for(id: &str) -> String {
        jwt_with_payload(&format!(r#"{{"sub":"{id}"}}"#))
    }

    /// 绑定成功：网页 field 写入、Code blob 不动、extra 三键写入且保留既有键、
    /// 到期日来自 nextBillingTime（不是 balances.expireTime 的 2026-09-28）、
    /// 周期天数来自 currentStartTime、
    /// 取消续费但 active 的订阅照常给日期且不产生取消语义、请求带 Connect 协议头。
    #[tokio::test]
    async fn bind_success_writes_web_field_extra_and_returns_expiry() {
        let h = Harness::new();
        h.seed("u1");
        let web_token = token_for("u1");
        let server = MockServer::start(vec![("200 OK", SUBSCRIPTION_FIXTURE)]);

        let binding = bind_web_at(&h.registry, &h.store, &AccountId("u1".into()), &web_token, server.base_url())
            .await
            .unwrap();

        // 到期日：nextBillingTime 2027-08-27（东八区日历日），不是干扰项 2026-09-28。
        assert_eq!(binding.expiry_text.as_deref(), Some("2027-08-27"));
        // 周期天数：开始日 2026-08-28 → 展示到期日 2027-08-27 = 364 天。
        assert_eq!(binding.period_days, Some(364));

        // 网页凭证独立 field；Code blob 原样。
        assert_eq!(
            h.store.get("kimi", "u1", WEB_STORE_FIELD).unwrap().as_deref(),
            Some(web_token.as_str())
        );
        assert_eq!(
            h.store.get("kimi", "u1", "blob").unwrap().as_deref(),
            Some(r#"{"uid":"u1","access_token":"A1"}"#)
        );

        // extra：三个 membership 键写入（字面量与官网响应一致），既有键保留，
        // 不落任何取消/档位语义。
        let bound = h.bound_account("u1");
        assert_eq!(
            bound.extra.get(EXTRA_CURRENT_START_TIME).and_then(|v| v.as_str()),
            Some("2026-08-28T00:00:00Z")
        );
        assert_eq!(
            bound.extra.get(EXTRA_CURRENT_END_TIME).and_then(|v| v.as_str()),
            Some("2027-08-28T00:00:00Z")
        );
        assert_eq!(
            bound.extra.get(EXTRA_NEXT_BILLING_TIME).and_then(|v| v.as_str()),
            Some("2027-08-27T01:25:37.930341Z")
        );
        assert_eq!(
            bound.extra.get("dedup_key").and_then(|v| v.as_str()),
            Some("stable-u1")
        );
        assert_eq!(
            bound.extra.get("scope").and_then(|v| v.as_str()),
            Some("kimi-code")
        );
        let extra_json = serde_json::to_string(&bound.extra).unwrap();
        assert!(!extra_json.contains("CANCEL"), "不产生取消语义：{extra_json}");
        assert!(!extra_json.contains("membershipLevel"), "档位不落库：{extra_json}");
        assert_eq!(
            membership_expiry_from_extra(&bound.extra).as_deref(),
            Some("2027-08-27")
        );

        // 请求形态：绑定时只打一次，路径与 Connect 协议头、UA、Bearer 都要带上。
        let requests = server.finish_full();
        assert_eq!(requests.len(), 1, "绑定时只请求一次 GetSubscription");
        let full = requests.join("\n").to_lowercase();
        assert!(
            full.contains("post /apiv2/kimi.gateway.membership.v2.membershipservice/getsubscription"),
            "请求行应为 GetSubscription 路径：{full}"
        );
        assert!(full.contains("connect-protocol-version: 1"), "{full}");
        assert!(full.contains("user-agent: kimi-switch"), "{full}");
        assert!(
            full.contains(&format!("authorization: bearer {}", web_token.to_lowercase())),
            "{full}"
        );
    }

    /// 身份不一致：英文错误（含双方 id、说明 nothing written、不含 token），
    /// 网页 field / extra / Code blob 相对绑定前逐项不变，且不打任何网络请求。
    #[tokio::test]
    async fn identity_mismatch_writes_nothing_and_makes_no_request() {
        let h = Harness::new();
        let seeded = h.seed("u1");
        // token 属于 u9，且尾部带泄漏标记：错误文案不得包含 token（含标记）。
        let web_token = format!("{}.sig-LEAK-MARKER", token_for("u9"));
        let server = MockServer::start(vec![]);

        let err = bind_web_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            &web_token,
            server.base_url(),
        )
        .await
        .unwrap_err();

        match err.bind_web_error() {
            Some(kimi_switch_core::BindWebError::IdentityMismatch { observed, target }) => {
                assert_eq!(observed, "u9");
                assert_eq!(target, "u1");
            }
            other => panic!("期望 BindWeb IdentityMismatch，得到 {other:?} / {err}"),
        }
        let message = err.to_string();
        assert!(message.contains("nothing written"), "{message}");
        assert!(!message.contains("LEAK-MARKER"), "错误不得含 token：{message}");
        assert!(!message.contains(&web_token), "{message}");

        // 库存逐项不变：网页 field、Code blob、registry 行（含 extra）。
        assert_eq!(h.store.get("kimi", "u1", WEB_STORE_FIELD).unwrap(), None);
        assert_eq!(
            h.store.get("kimi", "u1", "blob").unwrap().as_deref(),
            Some(r#"{"uid":"u1","access_token":"A1"}"#)
        );
        assert_eq!(h.bound_account("u1").extra, seeded.extra);
        assert_eq!(
            h.bound_account("u1").label,
            seeded.label,
            "registry 行整体不变"
        );
        assert!(server.finish().is_empty(), "身份门失败不得打网络请求");
    }

    /// 无法辨认（非 JWT / payload 无 sub 与 user_id）：同样失败且不写任何地方。
    #[tokio::test]
    async fn unidentifiable_token_writes_nothing() {
        let h = Harness::new();
        let seeded = h.seed("u1");
        let server = MockServer::start(vec![]);

        for bad in [
            "not-a-jwt".to_string(),
            jwt_with_payload("{}"),
            jwt_with_payload(r#"{"exp":1234567890}"#),
        ] {
            let err = bind_web_at(
                &h.registry,
                &h.store,
                &AccountId("u1".into()),
                &bad,
                server.base_url(),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(
                    err.bind_web_error(),
                    Some(kimi_switch_core::BindWebError::Unidentifiable)
                ),
                "无法辨认应透出 Unidentifiable：{err}"
            );
            assert!(!err.to_string().contains(&bad), "错误不得含 token：{err}");
        }

        assert_eq!(h.store.get("kimi", "u1", WEB_STORE_FIELD).unwrap(), None);
        assert_eq!(h.bound_account("u1").extra, seeded.extra);
        assert!(server.finish().is_empty());
    }

    /// 身份辨认回退顺序：无 `sub` 时用 `user_id`（网页 JWT 两种主键形态都认）。
    #[tokio::test]
    async fn identity_falls_back_to_user_id() {
        let h = Harness::new();
        h.seed("u1");
        let web_token = jwt_with_payload(r#"{"user_id":"u1"}"#);
        let server = MockServer::start(vec![("200 OK", SUBSCRIPTION_FIXTURE)]);

        let binding = bind_web_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            &web_token,
            server.base_url(),
        )
        .await
        .unwrap();

        assert_eq!(binding.expiry_text.as_deref(), Some("2027-08-27"));
        assert_eq!(
            h.store.get("kimi", "u1", WEB_STORE_FIELD).unwrap().as_deref(),
            Some(web_token.as_str())
        );
        assert_eq!(server.finish().len(), 1);
    }

    /// GetSubscription 失败（HTTP 500）：英文错误不含 token；extra 不动；
    /// 网页 field 保留（设计行为：身份已验证的 token 是有效绑定，ADR-0004）。
    #[tokio::test]
    async fn http_failure_keeps_web_field_but_leaves_extra_untouched() {
        let h = Harness::new();
        let seeded = h.seed("u1");
        let web_token = format!("{}.sig-LEAK-MARKER", token_for("u1"));
        let server = MockServer::start(vec![("500 Internal Server Error", r#"{"code":"boom"}"#)]);

        let err = bind_web_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            &web_token,
            server.base_url(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.http_status(), Some(500));
        let message = err.to_string();
        assert!(!message.contains("LEAK-MARKER"), "错误不得含 token：{message}");
        assert!(!message.contains(&web_token), "{message}");
        assert!(!message.contains("boom"), "不回显响应 body：{message}");

        assert_eq!(
            h.store.get("kimi", "u1", WEB_STORE_FIELD).unwrap().as_deref(),
            Some(web_token.as_str()),
            "网页凭证按写序保留（身份门已过）"
        );
        assert_eq!(
            h.bound_account("u1").extra,
            seeded.extra,
            "会员读取失败不写 extra"
        );
        assert_eq!(server.finish().len(), 1);
    }

    /// 重新绑定覆盖旧日期：新订阅没有 nextBillingTime 时旧键必须移除，
    /// 到期展示落到 currentEndTime 的 UTC 零点前一天。
    #[tokio::test]
    async fn rebinding_replaces_stale_expiry_keys() {
        let h = Harness::new();
        h.seed("u1");
        let first_server = MockServer::start(vec![("200 OK", SUBSCRIPTION_FIXTURE)]);
        bind_web_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            &token_for("u1"),
            first_server.base_url(),
        )
        .await
        .unwrap();
        assert!(h.bound_account("u1").extra.contains_key(EXTRA_NEXT_BILLING_TIME));
        assert!(h
            .bound_account("u1")
            .extra
            .contains_key(EXTRA_CURRENT_START_TIME));

        let second_server = MockServer::start(vec![("200 OK", CURRENT_END_ONLY_FIXTURE)]);
        let binding = bind_web_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            &token_for("u1"),
            second_server.base_url(),
        )
        .await
        .unwrap();

        let extra = h.bound_account("u1").extra;
        assert!(
            !extra.contains_key(EXTRA_NEXT_BILLING_TIME),
            "旧 nextBillingTime 不得残留：{extra:?}"
        );
        assert!(
            !extra.contains_key(EXTRA_CURRENT_START_TIME),
            "旧 currentStartTime 不得残留：{extra:?}"
        );
        assert_eq!(
            extra.get(EXTRA_CURRENT_END_TIME).and_then(|v| v.as_str()),
            Some("2028-08-28T00:00:00Z")
        );
        // UTC 零点 → 右开语义显示前一天。
        assert_eq!(binding.expiry_text.as_deref(), Some("2028-08-27"));
        assert_eq!(
            membership_expiry_from_extra(&extra).as_deref(),
            Some("2028-08-27")
        );
    }

    /// 目标账号不在 registry：在任何写入之前失败（AccountNotFound），不打网络。
    #[tokio::test]
    async fn missing_account_fails_before_any_write() {
        let h = Harness::new();
        let server = MockServer::start(vec![]);

        let err = bind_web_at(
            &h.registry,
            &h.store,
            &AccountId("ghost".into()),
            &token_for("ghost"),
            server.base_url(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.http_status(), None);
        assert!(
            matches!(err, kimi_switch_core::Error::AccountNotFound { .. }),
            "期望 AccountNotFound：{err}"
        );
        assert_eq!(h.store.get("kimi", "ghost", WEB_STORE_FIELD).unwrap(), None);
        assert!(server.finish().is_empty());
    }

    // --- 仅刷新路径（issue #52）---

    /// 刷新成功：重写 extra 三个键（旧日期换新、既有键保留）、网页凭证不动、
    /// 只请求一次、返回新到期展示日；缓存的旧日期已过期也照样请求（触发口径）。
    /// 新订阅无 currentStartTime → 旧开始键移除、周期天数为 None。
    #[tokio::test]
    async fn refresh_rewrites_extra_and_keeps_web_field() {
        let h = Harness::new();
        h.seed_bound("u1");
        let server = MockServer::start(vec![("200 OK", CURRENT_END_ONLY_FIXTURE)]);

        let binding = refresh_web_membership_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            server.base_url(),
        )
        .await
        .unwrap();

        assert_eq!(binding.expiry_text.as_deref(), Some("2028-08-27"));
        // 新订阅无 currentStartTime：周期天数缺省、旧开始键不残留。
        assert_eq!(binding.period_days, None);
        let bound = h.bound_account("u1");
        assert_eq!(
            bound.extra.get(EXTRA_CURRENT_END_TIME).and_then(|v| v.as_str()),
            Some("2028-08-28T00:00:00Z")
        );
        assert!(
            !bound.extra.contains_key(EXTRA_CURRENT_START_TIME),
            "旧 currentStartTime 不得残留：{:?}",
            bound.extra
        );
        assert!(
            !bound.extra.contains_key(EXTRA_NEXT_BILLING_TIME),
            "旧 nextBillingTime 不得残留：{:?}",
            bound.extra
        );
        assert_eq!(
            bound.extra.get("dedup_key").and_then(|v| v.as_str()),
            Some("stable-u1")
        );
        // 网页凭证原样（刷新不重写绑定）。
        assert_eq!(
            h.store.get("kimi", "u1", WEB_STORE_FIELD).unwrap().as_deref(),
            Some(token_for("u1").as_str())
        );
        // 只请求一次，且带 Connect 协议头。
        let requests = server.finish_full();
        assert_eq!(requests.len(), 1);
        assert!(
            requests
                .join("\n")
                .to_lowercase()
                .contains("connect-protocol-version: 1")
        );
    }

    /// 刷新后订阅已无到期数据：成功也清掉三个键（回到「无到期日」，不残留旧日期）。
    #[tokio::test]
    async fn refresh_success_without_dates_clears_stale_expiry() {
        let h = Harness::new();
        h.seed_bound("u1");
        let server = MockServer::start(vec![("200 OK", NO_EXPIRY_FIXTURE)]);

        let binding = refresh_web_membership_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            server.base_url(),
        )
        .await
        .unwrap();

        assert_eq!(binding.expiry_text, None);
        assert_eq!(binding.period_days, None);
        let extra = h.bound_account("u1").extra;
        assert!(!extra.contains_key(EXTRA_CURRENT_START_TIME));
        assert!(!extra.contains_key(EXTRA_CURRENT_END_TIME));
        assert!(!extra.contains_key(EXTRA_NEXT_BILLING_TIME));
        assert_eq!(membership_expiry_from_extra(&extra), None);
    }

    /// 刷新失败（401 类网页 token 失效）：英文错误不含 token；extra 保留旧
    /// 日期、网页凭证保留（缓存降级不删数据）；只请求一次。
    #[tokio::test]
    async fn refresh_failure_keeps_extra_and_web_field() {
        let h = Harness::new();
        let seeded = h.seed_bound("u1");
        let web_token = token_for("u1");
        let server = MockServer::start(vec![("401 Unauthorized", r#"{"code":"unauthorized"}"#)]);

        let err = refresh_web_membership_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            server.base_url(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.http_status(), Some(401));
        let message = err.to_string();
        assert!(
            !message.contains(&web_token),
            "错误不得含 token：{message}"
        );
        assert!(!message.contains("unauthorized}"), "不回显响应 body：{message}");

        // 缓存降级：extra 旧日期原样保留，网页凭证不动。
        assert_eq!(h.bound_account("u1").extra, seeded.extra);
        assert_eq!(
            h.store.get("kimi", "u1", WEB_STORE_FIELD).unwrap().as_deref(),
            Some(web_token.as_str())
        );
        assert_eq!(server.finish().len(), 1);
    }

    /// 未绑定（无网页凭证）：英文错误、不打网络、extra 不动。
    #[tokio::test]
    async fn refresh_without_binding_fails_without_request() {
        let h = Harness::new();
        let seeded = h.seed("u1");
        let server = MockServer::start(vec![]);

        let err = refresh_web_membership_at(
            &h.registry,
            &h.store,
            &AccountId("u1".into()),
            server.base_url(),
        )
        .await
        .unwrap_err();

        assert_eq!(err.http_status(), None);
        assert!(
            err.to_string()
                .contains("requires an existing web binding"),
            "英文错误说明缺少绑定：{err}"
        );
        assert_eq!(h.bound_account("u1").extra, seeded.extra);
        assert!(server.finish().is_empty());
    }

    /// 目标账号不在 registry：在任何网络请求之前失败。
    #[tokio::test]
    async fn refresh_missing_account_fails_before_request() {
        let h = Harness::new();
        let server = MockServer::start(vec![]);

        let err = refresh_web_membership_at(
            &h.registry,
            &h.store,
            &AccountId("ghost".into()),
            server.base_url(),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, kimi_switch_core::Error::AccountNotFound { .. }),
            "期望 AccountNotFound：{err}"
        );
        assert!(server.finish().is_empty());
    }

    /// 生产入口域名固定为 `www.kimi.com`（ADR-0004：不做环境变量覆写）。
    #[test]
    fn membership_base_url_is_pinned_to_www_kimi_com() {
        assert_eq!(MEMBERSHIP_BASE_URL, "https://www.kimi.com");
    }
}
