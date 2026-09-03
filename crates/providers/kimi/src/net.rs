//! Kimi 网络层共享发送入口：环境代理不可用时按回放安全性分级回退直连（ADR-0005）。
//! 第 1 次尝试与历史行为完全一致（reqwest 默认读取 `HTTP(S)_PROXY`/`ALL_PROXY`）；
//! 仅当环境代理存在且错误满足策略门槛时，改用 `.no_proxy()` 客户端重建请求重试一次。

use std::time::Duration;

/// 代理回退门槛：按端点回放安全性分级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProxyFallback {
    /// 任何 send 层错误都回退。只用于重放安全端点（GET、幂等或无副作用的 POST）。
    Always,
    /// 仅连接层错误回退。用于一次性 refresh token 的 POST：connect 类错误
    /// （TCP/DNS/TLS 握手、代理隧道建立失败）都发生在请求字节写出之前，直连
    /// 重试不可能双消费 token；超时等无法排除「已发出」的形态不回退，避免
    /// 「首次实际成功轮换、直连重放被拒 → 误标 DeadToken 锁死账号」。
    ConnectOnly,
}

/// Kimi 网络请求统一发送入口。
pub(crate) async fn send<F>(
    timeout: Option<Duration>,
    policy: ProxyFallback,
    build_request: F,
) -> reqwest::Result<reqwest::Response>
where
    F: Fn(reqwest::Client) -> reqwest::RequestBuilder,
{
    attempt_with_fallback(timeout, policy, None, env_proxy_present(), build_request).await
}

/// `attempt1_proxy`：测试注入显式代理模拟「代理不可达」；生产传 `None` 走 env 默认。
async fn attempt_with_fallback<F>(
    timeout: Option<Duration>,
    policy: ProxyFallback,
    attempt1_proxy: Option<&str>,
    env_proxy: bool,
    build_request: F,
) -> reqwest::Result<reqwest::Response>
where
    F: Fn(reqwest::Client) -> reqwest::RequestBuilder,
{
    let mut builder = reqwest::Client::builder();
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(proxy) = attempt1_proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    match build_request(builder.build()?).send().await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            if !should_fallback(policy, env_proxy, e.is_connect()) {
                return Err(e);
            }
            tracing::warn!(err = %e, "env proxy unusable; retrying kimi request without proxy");
            let mut direct = reqwest::Client::builder().no_proxy();
            if let Some(timeout) = timeout {
                direct = direct.timeout(timeout);
            }
            build_request(direct.build()?).send().await
        }
    }
}

/// 是否应回退直连。
fn should_fallback(policy: ProxyFallback, env_proxy: bool, err_is_connect: bool) -> bool {
    env_proxy
        && match policy {
            ProxyFallback::Always => true,
            ProxyFallback::ConnectOnly => err_is_connect,
        }
}

/// 环境是否配置了代理变量（大小写任一形态、非空白即算）。
fn env_proxy_present() -> bool {
    env_proxy_present_in(|name| std::env::var(name).ok())
}

fn env_proxy_present_in(lookup: impl Fn(&str) -> Option<String>) -> bool {
    const NAMES: [&str; 6] = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    NAMES
        .iter()
        .any(|name| lookup(name).is_some_and(|value| !value.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockServer;

    /// Windows 上请求级 env 代理无法注入（进程全局且 reqwest 有缓存），
    /// 用端口 1（无监听）作为确定性死代理。
    const DEAD_PROXY: &str = "http://127.0.0.1:1";

    #[test]
    fn should_fallback_matrix() {
        assert!(should_fallback(ProxyFallback::Always, true, false));
        assert!(should_fallback(ProxyFallback::Always, true, true));
        assert!(!should_fallback(ProxyFallback::Always, false, true));
        assert!(should_fallback(ProxyFallback::ConnectOnly, true, true));
        assert!(!should_fallback(ProxyFallback::ConnectOnly, true, false));
        assert!(!should_fallback(ProxyFallback::ConnectOnly, false, true));
    }

    #[test]
    fn env_proxy_present_in_covers_names_and_ignores_blank() {
        let pick = |name: &'static str, value: &'static str| {
            move |n: &str| (n == name).then(|| value.to_string())
        };
        assert!(env_proxy_present_in(pick("HTTPS_PROXY", "http://127.0.0.1:7897")));
        assert!(env_proxy_present_in(pick("https_proxy", "http://127.0.0.1:7897")));
        assert!(env_proxy_present_in(pick("ALL_PROXY", "socks5://127.0.0.1:1080")));
        assert!(env_proxy_present_in(pick("all_proxy", "http://p")));
        assert!(env_proxy_present_in(pick("HTTP_PROXY", "http://p")));
        assert!(env_proxy_present_in(pick("http_proxy", "http://p")));
        assert!(!env_proxy_present_in(pick("HTTP_PROXY", "  ")));
        assert!(!env_proxy_present_in(|_| None));
    }

    #[tokio::test]
    async fn dead_proxy_falls_back_to_direct_under_both_policies() {
        // 死代理的失败是 connect 型，两种策略都应回退并由直连拿到 MockServer 的 200。
        for policy in [ProxyFallback::Always, ProxyFallback::ConnectOnly] {
            let server = MockServer::start(vec![("200 OK", r#"{"device_code":"dc-1"}"#)]);
            let resp = attempt_with_fallback(
                Some(Duration::from_secs(5)),
                policy,
                Some(DEAD_PROXY),
                true,
                |client| client.post(server.base_url()),
            )
            .await
            .unwrap();
            assert!(resp.status().is_success());
        }
    }

    #[tokio::test]
    async fn no_env_proxy_means_no_fallback() {
        // 环境无代理变量时即使连接失败也不回退：行为与历史完全一致。
        let err = attempt_with_fallback(
            Some(Duration::from_secs(5)),
            ProxyFallback::Always,
            Some(DEAD_PROXY),
            false,
            |client| client.get("http://127.0.0.1:1/never"),
        )
        .await
        .unwrap_err();
        assert!(err.is_connect());
    }
}
