//! 解析 kimi-code.json：整段当 opaque blob，只从 access_token JWT 抽 user_id 等做展示。

use kimi_switch_engine::BlobMetadata;

/// 解析 base64url JWT payload。
pub fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64url_decode(payload)?;
    serde_json::from_slice(&decoded).ok()
}

/// access token 是否已过 JWT exp。无法确证（非 JWT、无 exp、exp 非数值）时返回 false：
/// 只有确证过期才把 401 归入「凭证过期型」，其余情况维持原分类。
pub fn jwt_expired(token: &str) -> bool {
    jwt_expired_at(token, chrono::Utc::now().timestamp())
}

/// RFC 7519：on or after exp 即不得接受，故 `now >= exp` 视为已过期。
fn jwt_expired_at(token: &str, now: i64) -> bool {
    let Some(exp) = jwt_exp_claim(token) else {
        return false;
    };
    now >= exp
}

/// 主动续期的轮换窗口（秒，issue #44）：access token 剩余不足此值即视为需要轮换。
const RENEWAL_SLACK_SECS: i64 = 60;

/// 主动续期的临期判定：已过期或剩余不足 60 秒 → 需要轮换。
/// 与 [`jwt_expired_at`] 同一 exp 读取口径；无法确证（非 JWT、无 exp、exp 非数值）
/// 时返回 false——不能确证临期就不主动消耗一次性 refresh token。
///
/// 目前仅测试引用：生产路径走 [`exp_needs_renewal`] 纯核（oauth 续期门把
/// IO/解析剥离后直接判定 exp），本包装保留给边界语义的 token 级回归测试。
#[cfg(test)]
pub fn jwt_needs_renewal(token: &str) -> bool {
    jwt_expires_within(token, chrono::Utc::now().timestamp(), RENEWAL_SLACK_SECS)
}

/// 临期判定的 exp 纯核（主动续期门把 IO/解析剥离后直接复用，slack 固定
/// [`RENEWAL_SLACK_SECS`]）：已过期或剩余不足 60 秒 → true；
/// exp 无法确证（None）→ false。与 `jwt_needs_renewal` 共用同一边界。
pub(crate) fn exp_needs_renewal(exp: Option<i64>, now: i64) -> bool {
    let Some(exp) = exp else {
        return false;
    };
    exp_within(exp, now, RENEWAL_SLACK_SECS)
}

/// 已过期（`now >= exp`，与 [`jwt_expired_at`] 同口径）或剩余不足 slack 秒
/// （`exp - now < slack_secs`）。仅测试与 [`jwt_needs_renewal`] 引用。
#[cfg(test)]
fn jwt_expires_within(token: &str, now: i64, slack_secs: i64) -> bool {
    let Some(exp) = jwt_exp_claim(token) else {
        return false;
    };
    exp_within(exp, now, slack_secs)
}

/// 唯一比较点：已过期（`now >= exp`）或剩余不足 slack 秒（`exp - now < slack_secs`）。
fn exp_within(exp: i64, now: i64, slack_secs: i64) -> bool {
    now >= exp || exp - now < slack_secs
}

/// 读取 JWT exp（整型或浮点 NumericDate 取整）；无法确证时返回 None。
pub(crate) fn jwt_exp_claim(token: &str) -> Option<i64> {
    let claims = decode_jwt_payload(token)?;
    let value = claims.get("exp")?;
    value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))
}

/// 凭证的过期时刻（Unix 秒）：优先取 access token 的 JWT `exp`，缺失时回退凭证里的
/// `expires_at`（整型或浮点取整）；都解析不出为 None。回灌新旧比对据此判断两份凭证谁更新
/// （ADR-0019）。
pub(crate) fn credential_expiry(blob: &str) -> Option<i64> {
    let value: serde_json::Value = serde_json::from_str(blob).ok()?;
    value
        .get("access_token")
        .and_then(|t| t.as_str())
        .and_then(jwt_exp_claim)
        .or_else(|| {
            let expires_at = value.get("expires_at")?;
            expires_at
                .as_i64()
                .or_else(|| expires_at.as_f64().map(|f| f as i64))
        })
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity((input.len() * 3) / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        while bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// 从 blob 抽元数据。主键 = user_id；label 缺省也用 user_id；无 email。
pub fn parse_metadata(blob: &str) -> BlobMetadata {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(blob) else {
        return BlobMetadata::default();
    };
    let claims = value
        .get("access_token")
        .and_then(|t| t.as_str())
        .and_then(decode_jwt_payload);
    let user_id = claims
        .as_ref()
        .and_then(|c| c.get("user_id"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut extra = serde_json::Map::new();
    if let Some(scope) = value.get("scope").and_then(|v| v.as_str()) {
        extra.insert("scope".into(), serde_json::Value::String(scope.into()));
    }

    BlobMetadata {
        primary_id: user_id.clone(),
        label: user_id,
        dedup_key: None,
        extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // header.payload.sig；payload = {"user_id":"u-123","client_id":"c-1","scope":"kimi-code"}
    const JWT: &str = "eyJhbGciOiJFUzI1NiJ9.eyJ1c2VyX2lkIjoidS0xMjMiLCJjbGllbnRfaWQiOiJjLTEiLCJzY29wZSI6ImtpbWktY29kZSJ9.sig";

    #[test]
    fn parses_user_id_from_jwt() {
        let blob = format!(r#"{{"access_token":"{JWT}","scope":"kimi-code"}}"#);
        let m = parse_metadata(&blob);
        assert_eq!(m.primary_id.as_deref(), Some("u-123"));
        assert_eq!(m.label.as_deref(), Some("u-123"));
        assert_eq!(
            m.extra.get("scope").and_then(|v| v.as_str()),
            Some("kimi-code")
        );
    }

    #[test]
    fn garbage_is_empty_metadata() {
        assert!(parse_metadata("not json").primary_id.is_none());
    }

    #[test]
    fn jwt_expired_at_reads_numeric_exp_claim() {
        let expired = crate::test_support::jwt_with_payload(r#"{"user_id":"u-123","exp":1000}"#);
        assert!(jwt_expired_at(&expired, 2000));
        // 恰好在 exp 时刻已失效（RFC 7519：on or after exp 不得接受）。
        assert!(jwt_expired_at(&expired, 1000));
        let fresh = crate::test_support::jwt_with_payload(r#"{"user_id":"u-123","exp":3000}"#);
        assert!(!jwt_expired_at(&fresh, 2000));
        // 浮点 NumericDate 同样可取整判定。
        let float_exp = crate::test_support::jwt_with_payload(r#"{"exp":1000.9}"#);
        assert!(jwt_expired_at(&float_exp, 2000));
        assert!(!jwt_expired_at(&float_exp, 999));
    }

    #[test]
    fn jwt_expired_at_is_false_when_expiry_unprovable() {
        // 无 exp、非 JWT、exp 非数值：都不能确证过期，维持原 401 分类。
        let no_exp = crate::test_support::jwt_with_payload(r#"{"user_id":"u-123"}"#);
        assert!(!jwt_expired_at(&no_exp, i64::MAX));
        assert!(!jwt_expired_at("not-a-jwt", i64::MAX));
        let text_exp = crate::test_support::jwt_with_payload(r#"{"exp":"soon"}"#);
        assert!(!jwt_expired_at(&text_exp, i64::MAX));
    }

    #[test]
    fn jwt_expires_within_covers_expired_and_slack_window() {
        let now = 2_000_000_000_i64;
        let at = |exp: i64| crate::test_support::jwt_with_payload(&format!(r#"{{"exp":{exp}}}"#));
        // 已过期（now >= exp，与 jwt_expired_at 同口径）→ 轮换。
        assert!(jwt_expires_within(&at(now - 1), now, 60));
        assert!(jwt_expires_within(&at(now), now, 60));
        // 剩余不足 60 秒 → 轮换。
        assert!(jwt_expires_within(&at(now + 59), now, 60));
        // 恰好剩余 60 秒：不满足「不足 60 秒」→ 不轮换。
        assert!(!jwt_expires_within(&at(now + 60), now, 60));
        assert!(!jwt_expires_within(&at(now + 61), now, 60));
        // 浮点 exp 取整后同口径。
        let float_exp = crate::test_support::jwt_with_payload(r#"{"exp":2000000059.9}"#);
        assert!(jwt_expires_within(&float_exp, now, 60));
    }

    #[test]
    fn jwt_needs_renewal_is_false_when_expiry_unprovable() {
        // 无法确证临期就不主动消耗一次性 refresh token。
        let no_exp = crate::test_support::jwt_with_payload(r#"{"user_id":"u-123"}"#);
        assert!(!jwt_needs_renewal(&no_exp));
        assert!(!jwt_needs_renewal("not-a-jwt"));
        // 远期未过期 → 不轮换；早已过期 → 轮换（用久远时间点避免依赖真实时钟精度）。
        let fresh = crate::test_support::jwt_with_payload(r#"{"exp":4000000000}"#);
        assert!(!jwt_needs_renewal(&fresh));
        let expired = crate::test_support::jwt_with_payload(r#"{"exp":1000000000}"#);
        assert!(jwt_needs_renewal(&expired));
    }

    /// 回灌新旧比对的过期时刻（ADR-0019）：JWT `exp` 优先，缺失时回退 `expires_at`。
    #[test]
    fn credential_expiry_prefers_jwt_exp_then_falls_back_to_expires_at() {
        let jwt = crate::test_support::jwt_with_payload;
        let blob = |access: &str, tail: &str| {
            format!(r#"{{"access_token":"{access}","refresh_token":"R1"{tail}}}"#)
        };

        assert_eq!(
            credential_expiry(&blob(
                &jwt(r#"{"user_id":"u-1","exp":1000}"#),
                r#","expires_at":2000"#
            )),
            Some(1000),
            "JWT exp 优先于 expires_at"
        );
        assert_eq!(
            credential_expiry(&blob(&jwt(r#"{"user_id":"u-1"}"#), r#","expires_at":2000"#)),
            Some(2000),
            "JWT 没有 exp 时回退 expires_at"
        );
        assert_eq!(
            credential_expiry(&blob("opaque", r#","expires_at":2000.7"#)),
            Some(2000),
            "access token 不是 JWT 时回退 expires_at，浮点取整"
        );
        assert_eq!(
            credential_expiry(&blob(&jwt(r#"{"user_id":"u-1"}"#), "")),
            None,
            "两处都没有：解析不出"
        );
        assert_eq!(credential_expiry("not json"), None);
    }

    #[test]
    fn jwt_payload_encoder_matches_known_good_vector() {
        // 与文件头既有常量同一 payload，钉住测试编码器本身的正确性。
        assert_eq!(
            crate::test_support::jwt_with_payload(r#"{"user_id":"u-123","client_id":"c-1"}"#),
            "header.eyJ1c2VyX2lkIjoidS0xMjMiLCJjbGllbnRfaWQiOiJjLTEifQ.sig"
        );
    }
}
