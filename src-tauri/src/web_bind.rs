//! Edge 免粘贴绑定的一次性回调 broker（issue #171）。
//!
//! 安全不变量：挑战 URL 只携带 nonce/端口元数据；回调监听只绑 loopback、
//! 限制方法/路径/大小并校验 nonce；一次合法回调只交付一次；取消、超时、
//! 重复回调、错误请求和跨账号串线都不交付、不写凭证；token 不进入日志、
//! 错误文本或返回给 WebView 的结构体。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rand::TryRngCore;
use serde::Serialize;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

const CALLBACK_PATH: &str = "/kimi-switch/web-bind";
const CONSOLE_URL: &str = "https://www.kimi.com/code/console";
const WAIT_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct WebBindChallenge {
    pub nonce: String,
    pub url: String,
}

struct Pending {
    target: String,
    receiver: Mutex<Option<oneshot::Receiver<Result<String>>>>,
    /// 取消信号用电平触发的 watch：无论监听任务是否已开始等待，取消后都能观察到。
    cancel_tx: watch::Sender<bool>,
}

pub struct WebBindBroker {
    pending: Mutex<HashMap<String, Arc<Pending>>>,
    /// 会话时长；生产固定 90s，测试注入更短值以覆盖超时路径。
    wait_timeout: Duration,
}

impl Default for WebBindBroker {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            wait_timeout: WAIT_TIMEOUT,
        }
    }
}

impl WebBindBroker {
    pub async fn begin(&self, target: &str) -> Result<WebBindChallenge> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("无法启动 Edge 绑定回调")?;
        let port = listener
            .local_addr()
            .context("无法读取 Edge 绑定回调地址")?
            .port();
        let nonce = random_nonce()?;
        let (sender, receiver) = oneshot::channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let pending = Arc::new(Pending {
            target: target.to_string(),
            receiver: Mutex::new(Some(receiver)),
            cancel_tx,
        });
        self.pending
            .lock()
            .expect("web bind pending lock")
            .insert(nonce.clone(), pending.clone());

        let task_nonce = nonce.clone();
        let timeout = self.wait_timeout;
        tokio::spawn(async move {
            let result = serve_callback(listener, task_nonce, cancel_rx, timeout).await;
            let _ = sender.send(result);
        });

        Ok(WebBindChallenge {
            nonce: nonce.clone(),
            url: format!("{CONSOLE_URL}#kimi-switch-bind=v1.{nonce}.{port}"),
        })
    }

    /// 等待回调并只交付一次。`expected_target` 必须等于 `begin` 指定的目标账号：
    /// 跨账号串线时丢弃回调凭证并报错，token 不离开 broker。
    /// 成功、失败、超时、取消都会作废该 nonce，会话不可二次消费。
    pub async fn wait(&self, nonce: &str, expected_target: &str) -> Result<String> {
        let pending = self
            .pending
            .lock()
            .expect("web bind pending lock")
            .get(nonce)
            .cloned()
            .ok_or_else(|| anyhow!("Edge 绑定会话不存在或已过期"))?;
        let receiver = pending
            .receiver
            .lock()
            .expect("web bind receiver lock")
            .take()
            .ok_or_else(|| anyhow!("Edge 绑定会话已被消费"))?;
        let mut cancel_rx = pending.cancel_tx.subscribe();
        let delivered: Result<String> = tokio::select! {
            value = tokio::time::timeout(self.wait_timeout, receiver) => match value {
                Err(_) => Err(anyhow!("Edge 绑定等待超时")),
                Ok(Err(_)) => Err(anyhow!("Edge 绑定回调已断开")),
                Ok(Ok(result)) => result,
            },
            _ = wait_cancelled(&mut cancel_rx) => Err(anyhow!("Edge 绑定已取消")),
        };
        self.pending
            .lock()
            .expect("web bind pending lock")
            .remove(nonce);
        let token = delivered?;
        if pending.target != expected_target {
            return Err(anyhow!("Edge 绑定目标已变化，请重试"));
        }
        Ok(token)
    }

    /// 作废一个等待中的绑定会话并关闭回调监听；返回该会话是否仍存在。
    pub fn cancel(&self, nonce: &str) -> bool {
        let pending = self
            .pending
            .lock()
            .expect("web bind pending lock")
            .remove(nonce);
        if let Some(pending) = pending {
            let _ = pending.cancel_tx.send(true);
            true
        } else {
            false
        }
    }
}

fn random_nonce() -> Result<String> {
    let mut bytes = [0_u8; 24];
    rand::rng()
        .try_fill_bytes(&mut bytes)
        .map_err(|error| anyhow!("无法生成 Edge 绑定随机数：{error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn serve_callback(
    listener: TcpListener,
    nonce: String,
    mut cancelled: watch::Receiver<bool>,
    timeout: Duration,
) -> Result<String> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted.context("Edge 绑定回调连接失败")?,
            _ = &mut deadline => return Err(anyhow!("Edge 绑定等待超时")),
            _ = wait_cancelled(&mut cancelled) => return Err(anyhow!("Edge 绑定已取消")),
        };
        // 会话剩余预算约束整条连接的处理：静默连接（建连不发数据）或声明
        // Content-Length 后少发正文，都不得把任务拖过截止时间（F3）。
        let budget = deadline.deadline() - tokio::time::Instant::now();
        let handled = tokio::time::timeout(budget, handle_callback(stream, &nonce)).await;
        match handled {
            Err(_) => return Err(anyhow!("Edge 绑定等待超时")),
            Ok(Ok(token)) => return Ok(token),
            // 合法回调但无法完成绑定（页面没有可用凭证）：终止会话，错误直达等待方（F1）。
            Ok(Err(CallbackError::Terminated(error))) => return Err(error),
            // 无效请求（方法/路径/nonce/大小/格式/连接中断）：忽略并继续等待。
            Ok(Err(CallbackError::Ignored(error))) => {
                tracing::debug!(error = %error, "ignored invalid Edge web bind callback");
            }
        }
    }
}

/// 等待取消标志。先查当前值，覆盖「订阅早于等待、取消先于等待」的窗口。
async fn wait_cancelled(cancelled: &mut watch::Receiver<bool>) {
    while !*cancelled.borrow() {
        if cancelled.changed().await.is_err() {
            // sender 已随会话释放，不可能再取消。
            return;
        }
    }
}

/// 回调处理的失败分类：
/// - `Terminated`：nonce 正确的合法回调但无法完成绑定（页面没有可用凭证），
///   会话就此终止，错误直达等待方，后续回调不再接受；
/// - `Ignored`：无效请求（错误方法/路径/nonce、超限、格式非法、连接中断），
///   忽略并继续等待合法回调，不推进绑定状态。
enum CallbackError {
    Terminated(anyhow::Error),
    Ignored(anyhow::Error),
}

impl From<std::io::Error> for CallbackError {
    fn from(error: std::io::Error) -> Self {
        CallbackError::Ignored(error.into())
    }
}

impl From<anyhow::Error> for CallbackError {
    fn from(error: anyhow::Error) -> Self {
        CallbackError::Ignored(error)
    }
}

/// 处理一条回调连接：只接受 `POST /kimi-switch/web-bind`、限头 16KB/正文 64KB、
/// 校验 nonce；nonce 正确但 token 为空属于终止性错误（页面没有可用凭证），
/// 其余非法请求为可忽略错误（文本均不含凭证内容）。流参数泛型化以便测试注入。
async fn handle_callback<S>(stream: S, expected_nonce: &str) -> Result<String, CallbackError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        // 对端未发任何数据就关闭：必须显式失败，否则 read_line 在 EOF 上
        // 反复返回空串会无限空转并卡死整个监听。
        return Err(anyhow!("Edge 回调连接已关闭").into());
    }
    let mut content_length = None;
    let mut header_bytes = request_line.len();
    if header_bytes > MAX_HEADER_BYTES {
        return Err(anyhow!("Edge 回调请求头过大").into());
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(anyhow!("Edge 回调连接已关闭").into());
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(anyhow!("Edge 回调请求头过大").into());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse::<usize>().context("非法回调长度")?);
            }
        }
    }
    let request = request_line.split_whitespace().collect::<Vec<_>>();
    if request.len() < 2 || request[0] != "POST" || request[1] != CALLBACK_PATH {
        return Err(anyhow!("非法 Edge 回调路径").into());
    }
    let length = content_length.ok_or_else(|| anyhow!("Edge 回调缺少长度"))?;
    if length == 0 || length > MAX_BODY_BYTES {
        return Err(anyhow!("Edge 回调正文过大或为空").into());
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).await?;
    let payload: CallbackPayload = serde_json::from_slice(&body).context("非法 Edge 回调正文")?;
    if payload.nonce != expected_nonce {
        return Err(anyhow!("Edge 回调 nonce 不匹配").into());
    }
    let token = payload.token.trim();
    if token.is_empty() {
        // nonce 正确但页面没有 token（未登录/无凭证）：立即失败直达等待方，
        // 不让用户等满 90s 泛化超时（issue #171 / #172 失败语义）。
        return Err(CallbackError::Terminated(anyhow!("Edge 页面没有可用网页凭证")));
    }
    let mut stream = reader.into_inner();
    stream
        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
        .await?;
    stream.shutdown().await?;
    Ok(token.to_string())
}

#[derive(Debug, serde::Deserialize)]
struct CallbackPayload {
    nonce: String,
    token: String,
}

#[cfg(windows)]
pub fn open_in_edge(url: &str) -> Result<()> {
    let mut candidates = Vec::new();
    for variable in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(variable) {
            candidates
                .push(std::path::PathBuf::from(root).join("Microsoft/Edge/Application/msedge.exe"));
        }
    }
    candidates.push(std::path::PathBuf::from("msedge.exe"));
    for candidate in candidates {
        let result = std::process::Command::new(&candidate).arg(url).spawn();
        if result.is_ok() {
            return Ok(());
        }
    }
    Err(anyhow!(
        "找不到 Microsoft Edge，请先安装 Edge；也可使用手动粘贴"
    ))
}

#[cfg(not(windows))]
pub fn open_in_edge(_url: &str) -> Result<()> {
    Err(anyhow!("Edge 自动绑定仅支持 Windows；可使用手动粘贴"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// 合法回调携带的合成网页凭证；任何错误文本、断言之外的位置都不得出现它。
    const TEST_TOKEN: &str = "SENSITIVE-web-token-never-leak-9f8e7d6c";
    /// 无效请求里携带的合成凭证，用于证明无效请求永远不会被交付。
    const INTRUDER_TOKEN: &str = "INTRUDER-web-token-must-be-dropped-1a2b3c";

    /// 测试用 broker：注入短会话时长，让失败路径快速失败而不是挂 90s。
    fn test_broker(timeout: Duration) -> WebBindBroker {
        WebBindBroker {
            wait_timeout: timeout,
            ..Default::default()
        }
    }

    /// 从挑战 URL fragment 提取回调端口：`v1.<48hex>.<port>` 的最后一段。
    fn challenge_port(challenge: &WebBindChallenge) -> u16 {
        let fragment = challenge.url.split('#').nth(1).expect("URL 含 fragment");
        let meta = fragment
            .strip_prefix("kimi-switch-bind=v1.")
            .expect("fragment 版本前缀");
        let (_, port) = meta.rsplit_once('.').expect("nonce.port 结构");
        port.parse().expect("端口为数字")
    }

    fn body_for(nonce: &str, token: &str) -> String {
        serde_json::json!({ "nonce": nonce, "token": token }).to_string()
    }

    fn post_with(method: &str, path: &str, nonce: &str, token: &str) -> Vec<u8> {
        let body = body_for(nonce, token);
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn post_request(nonce: &str, token: &str) -> Vec<u8> {
        post_with("POST", CALLBACK_PATH, nonce, token)
    }

    /// 向 loopback 回调端口发送原始字节，返回服务端响应（无效请求得到空串）。
    async fn send_raw(port: u16, raw: Vec<u8>) -> String {
        let mut stream = match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => stream,
            Err(_) => return String::new(),
        };
        let _ = stream.write_all(&raw).await;
        let _ = stream.shutdown().await;
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response).await;
        String::from_utf8_lossy(&response).into_owned()
    }

    /// 挑战 URL 必须精确等于「控制台 origin + v1 + nonce + 端口」，
    /// fragment 只含元数据，不含任何凭证或多余字段。
    #[tokio::test(flavor = "multi_thread")]
    async fn challenge_fragment_contains_only_metadata() {
        let broker = WebBindBroker::default();
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        assert_eq!(
            challenge.url,
            format!("{CONSOLE_URL}#kimi-switch-bind=v1.{}.{}", challenge.nonce, port)
        );
        assert_eq!(challenge.nonce.len(), 48);
        assert!(challenge.nonce.bytes().all(|b| b.is_ascii_hexdigit()));
        broker.cancel(&challenge.nonce);
    }

    /// 有效 nonce + 合法 POST 只交付一次，且会话随即作废。
    #[tokio::test(flavor = "multi_thread")]
    async fn valid_callback_delivers_once_then_session_consumed() {
        let broker = test_broker(Duration::from_secs(10));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        let token = broker.wait(&challenge.nonce, "alice").await.unwrap();
        assert_eq!(token, TEST_TOKEN);
        let response = send.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 204"), "合法回调应收到 204：{response}");

        // 一次性消费：nonce 已作废，重复 wait 与 cancel 都无会话可用。
        assert!(broker.wait(&challenge.nonce, "alice").await.is_err());
        assert!(!broker.cancel(&challenge.nonce));
    }

    /// 错误方法、路径、nonce、超限大小、非法 JSON 均不交付，
    /// 且监听继续存活：随后的合法回调仍可交付。（空 token 属终止性错误，
    /// 由 empty_token_callback_terminates_session 单独覆盖。）
    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_callbacks_do_not_deliver() {
        let nonce_hex = "f".repeat(48);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("错误方法", post_with("GET", CALLBACK_PATH, &nonce_hex, INTRUDER_TOKEN)),
            ("错误路径", post_with("POST", "/kimi-switch/other", &nonce_hex, INTRUDER_TOKEN)),
            ("错误 nonce", post_request(&"0".repeat(48), INTRUDER_TOKEN)),
            (
                "非法 JSON",
                format!(
                    "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 9\r\n\r\nnot-json?"
                )
                .into_bytes(),
            ),
            (
                "缺少长度",
                format!("POST {CALLBACK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").into_bytes(),
            ),
            (
                "超限正文",
                format!(
                    "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
                    MAX_BODY_BYTES + 1
                )
                .into_bytes(),
            ),
            (
                "超限请求头",
                format!(
                    "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pad: {}\r\nContent-Length: 4\r\n\r\nabcd",
                    "a".repeat(MAX_HEADER_BYTES)
                )
                .into_bytes(),
            ),
        ];
        for (label, raw) in cases {
            let broker = test_broker(Duration::from_secs(10));
            let challenge = broker.begin("alice").await.unwrap();
            let port = challenge_port(&challenge);
            // 先发无效请求（携带串扰凭证），必须被忽略且不推进会话。
            send_raw(port, raw.clone()).await;
            // 再发合法回调：交付的必须是合法凭证，证明无效请求没有交付、监听也未死。
            let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
            let delivered = broker
                .wait(&challenge.nonce, "alice")
                .await
                .unwrap_or_else(|error| panic!("{label}：合法回调应仍可交付：{error}"));
            assert_eq!(delivered, TEST_TOKEN, "{label}：不得交付串扰凭证");
            send.await.unwrap();
        }
    }

    /// 同一 nonce 的重复回调不得二次交付：监听已随首次交付关闭。
    #[tokio::test(flavor = "multi_thread")]
    async fn repeat_callback_is_not_delivered_twice() {
        let broker = test_broker(Duration::from_secs(10));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        assert_eq!(broker.wait(&challenge.nonce, "alice").await.unwrap(), TEST_TOKEN);
        send.await.unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        let repeat = send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)).await;
        assert!(!repeat.contains("204"), "重复回调不得再被消费：{repeat}");
        assert!(broker.wait(&challenge.nonce, "alice").await.is_err());
    }

    /// 空连接 / 半截请求不得把监听卡死（EOF 空转回归测试）。
    #[tokio::test(flavor = "multi_thread")]
    async fn empty_connection_does_not_wedge_listener() {
        let broker = test_broker(Duration::from_secs(5));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        // 连接后立即关闭写端，不发任何字节。
        let mut empty = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        empty.shutdown().await.unwrap();
        drop(empty);
        // 只发半截 header 就关闭。
        let mut partial = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        partial
            .write_all(b"POST /kimi-switch/web-bind HTTP/1.1\r\nContent-Len")
            .await
            .unwrap();
        partial.shutdown().await.unwrap();
        drop(partial);

        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        let token = broker.wait(&challenge.nonce, "alice").await.unwrap();
        assert_eq!(token, TEST_TOKEN);
        send.await.unwrap();
    }

    /// nonce 正确但 token 为空（未登录/页面无凭证）：终止性错误立即直达等待方，
    /// 会话终结、后续回调不再接受，而不是等满 90s 泛化超时（#172 失败语义）。
    #[tokio::test(flavor = "multi_thread")]
    async fn empty_token_callback_terminates_session() {
        let broker = test_broker(Duration::from_secs(10));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, "   ")));
        let error = tokio::time::timeout(Duration::from_secs(2), broker.wait(&challenge.nonce, "alice"))
            .await
            .expect("空 token 回调必须立即失败，而非等待超时")
            .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("没有可用网页凭证"), "应直达页面无凭证错误：{text}");
        assert!(!text.contains(TEST_TOKEN) && !text.contains(INTRUDER_TOKEN));
        send.await.unwrap();

        // 会话已终结：后续合法回调不再被接受，nonce 已作废。
        tokio::time::sleep(Duration::from_millis(50)).await;
        let response = send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)).await;
        assert!(!response.contains("204"), "终结后回调不得被消费：{response}");
        assert!(broker.wait(&challenge.nonce, "alice").await.is_err());
        assert!(!broker.cancel(&challenge.nonce));
    }

    /// 静默连接（建连后既不发数据也不关闭）不得把任务拖过会话预算：
    /// 会话必须在（注入的短）预算内收口，监听随之关闭（F3 回归）。
    #[tokio::test(flavor = "multi_thread")]
    async fn silent_connection_ends_within_budget() {
        let broker = Arc::new(test_broker(Duration::from_millis(300)));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        // 建连后保持静默：不发送任何字节，也不关闭写端。
        let _silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let waiter = broker.clone();
        let nonce = challenge.nonce.clone();
        let wait_handle = tokio::spawn(async move { waiter.wait(&nonce, "alice").await });
        let error = tokio::time::timeout(Duration::from_secs(2), wait_handle)
            .await
            .expect("静默连接不得拖过会话预算")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("超时"), "应为超时：{error}");

        // 收口后监听随会话关闭：新连接既得不到响应，也连不上端口。
        tokio::time::sleep(Duration::from_millis(100)).await;
        let response = send_raw(port, post_request(&challenge.nonce, INTRUDER_TOKEN)).await;
        assert!(!response.contains("204"), "收口后回调不得被消费：{response}");
        assert!(
            TcpStream::connect(("127.0.0.1", port)).await.is_err(),
            "会话结束后监听应已关闭"
        );
    }

    /// 声明 Content-Length 后少发正文的连接同样受会话预算约束（F3）。
    #[tokio::test(flavor = "multi_thread")]
    async fn short_body_connection_ends_within_budget() {
        let broker = Arc::new(test_broker(Duration::from_millis(300)));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        let mut partial_body = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        partial_body
            .write_all(
                format!(
                    "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 100\r\n\r\n{{\"nonce\"",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        // 不关闭连接、不补齐正文，保持挂起。

        let waiter = broker.clone();
        let nonce = challenge.nonce.clone();
        let wait_handle = tokio::spawn(async move { waiter.wait(&nonce, "alice").await });
        let error = tokio::time::timeout(Duration::from_secs(2), wait_handle)
            .await
            .expect("残缺正文连接不得拖过会话预算")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("超时"), "应为超时：{error}");
    }

    /// 等待超时不交付，错误文本不含凭证，nonce 随之作废。
    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_does_not_deliver_or_leak() {
        let broker = test_broker(Duration::from_millis(150));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        // 超时前送入一个携带串扰凭证的无效回调。
        send_raw(port, post_request(&"0".repeat(48), INTRUDER_TOKEN)).await;
        let error = broker.wait(&challenge.nonce, "alice").await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("超时"), "应为超时错误：{text}");
        assert!(!text.contains(INTRUDER_TOKEN) && !text.contains(TEST_TOKEN));
        // 会话已统一清理。
        assert!(!broker.cancel(&challenge.nonce));
    }

    /// 等待前取消：会话不存在，监听随即关闭，合法回调也无从交付。
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_before_wait_closes_listener() {
        let broker = test_broker(Duration::from_secs(10));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        assert!(broker.cancel(&challenge.nonce));
        let error = broker.wait(&challenge.nonce, "alice").await.unwrap_err();
        assert!(error.to_string().contains("不存在"), "会话应已作废：{error}");

        // 早取消也必须尽快关闭监听（不得残留到 90s 超时）。
        tokio::time::sleep(Duration::from_millis(50)).await;
        let response = send_raw(port, post_request(&challenge.nonce, INTRUDER_TOKEN)).await;
        assert!(!response.contains("204"), "取消后合法回调也不得被消费：{response}");
    }

    /// 等待中取消：等待方收到取消错误，此后再发的合法回调也不交付。
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_while_waiting_does_not_deliver() {
        let broker = Arc::new(test_broker(Duration::from_secs(10)));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        let waiter = broker.clone();
        let nonce = challenge.nonce.clone();
        let wait_handle = tokio::spawn(async move { waiter.wait(&nonce, "alice").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(broker.cancel(&challenge.nonce));

        let error = wait_handle.await.unwrap().unwrap_err();
        let text = error.to_string();
        assert!(text.contains("取消"), "应为取消错误：{text}");
        assert!(!text.contains(TEST_TOKEN) && !text.contains(INTRUDER_TOKEN));

        tokio::time::sleep(Duration::from_millis(50)).await;
        let response = send_raw(port, post_request(&challenge.nonce, INTRUDER_TOKEN)).await;
        assert!(!response.contains("204"), "取消后合法回调也不得被消费：{response}");
    }

    /// 跨账号串线：目标账号已变化时丢弃回调凭证并报错，token 不出现在错误文本。
    #[tokio::test(flavor = "multi_thread")]
    async fn cross_account_callback_is_rejected() {
        let broker = test_broker(Duration::from_secs(10));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        // 用另一个账号身份消费：必须拒绝，凭证被丢弃而不是转交。
        let error = broker.wait(&challenge.nonce, "bob").await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("目标已变化"), "应为串线拒绝：{text}");
        assert!(!text.contains(TEST_TOKEN), "错误文本不得包含凭证");
        send.await.unwrap();

        // 串线已消费该一次性会话：原账号也不能再拿回这个 token。
        let retry = broker.wait(&challenge.nonce, "alice").await.unwrap_err();
        assert!(retry.to_string().contains("不存在"));
    }

    /// handle_callback 的错误分类与文本：空 token 为终止性错误，其余为可忽略
    /// 错误；两类错误文本都不得包含请求中的凭证内容（合成数据验证）。
    #[tokio::test(flavor = "multi_thread")]
    async fn handle_callback_errors_never_contain_token() {
        let nonce = "f".repeat(48);
        // (标签, 请求字节, 是否应为终止性错误)
        let cases: Vec<(&str, Vec<u8>, bool)> = vec![
            ("错误 nonce", post_request(&"0".repeat(48), INTRUDER_TOKEN), false),
            ("空 token", post_request(&nonce, "   "), true),
            ("错误方法", post_with("PUT", CALLBACK_PATH, &nonce, INTRUDER_TOKEN), false),
            ("错误路径", post_with("POST", "/evil", &nonce, INTRUDER_TOKEN), false),
            (
                "非法 JSON",
                format!(
                    "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{INTRUDER_TOKEN}",
                    INTRUDER_TOKEN.len()
                )
                .into_bytes(),
                false,
            ),
            (
                "超限正文",
                format!(
                    "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
                    MAX_BODY_BYTES + 1
                )
                .into_bytes(),
                false,
            ),
            (
                "超限请求头",
                format!(
                    "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pad: {}\r\nContent-Length: 4\r\n\r\nabcd",
                    "a".repeat(MAX_HEADER_BYTES)
                )
                .into_bytes(),
                false,
            ),
            ("空连接", Vec::new(), false),
            (
                "半截请求",
                b"POST /kimi-switch/web-bind HTTP/1.1\r\nContent-Len".to_vec(),
                false,
            ),
        ];
        for (label, raw, expect_terminal) in cases {
            let (mut client, server) = tokio::io::duplex(128 * 1024);
            let _ = client.write_all(&raw).await;
            let _ = client.shutdown().await;
            // 若退化成 EOF 空转，这里会以超时失败而不是挂死测试线程。
            let handled =
                tokio::time::timeout(Duration::from_secs(2), handle_callback(server, &nonce)).await;
            let error = handled
                .unwrap_or_else(|_| panic!("{label}：handle_callback 不得空转"))
                .unwrap_err();
            let text = match &error {
                CallbackError::Terminated(error) => {
                    assert!(expect_terminal, "{label}：应为可忽略错误，实际为终止性");
                    error.to_string()
                }
                CallbackError::Ignored(error) => {
                    assert!(!expect_terminal, "{label}：应为终止性错误，实际为可忽略");
                    error.to_string()
                }
            };
            assert!(
                !text.contains(INTRUDER_TOKEN),
                "{label}：错误文本泄漏凭证：{text}"
            );
        }
    }

    #[test]
    fn nonce_is_not_reused() {
        let a = random_nonce().unwrap();
        let b = random_nonce().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 48);
    }
}
