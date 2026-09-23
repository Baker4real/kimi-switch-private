//! Edge 免粘贴绑定的一次性回调 broker（issue #171）。
//!
//! 安全不变量：挑战 URL 只携带 nonce/端口元数据；回调监听只绑 loopback、
//! 限制方法/路径/大小并校验 nonce；一次合法回调只交付一次；取消、超时、
//! 重复回调、错误请求和跨账号串线都不交付、不写凭证；token 不进入日志、
//! 错误文本、状态回查载荷或返回给 WebView 的结构体。
//!
//! 状态回查（issue #178）是额外的只读 GET [`STATUS_PATH`]：凭 nonce、无副作用。
//! 公开失败原因只有四句。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rand::TryRngCore;
use serde::Serialize;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

const CALLBACK_PATH: &str = "/kimi-switch/web-bind";
/// 状态回查路径。必须与 [`CALLBACK_PATH`] 不同，GET 不能打进 POST 回调。
const STATUS_PATH: &str = "/kimi-switch/web-bind-status";
const CONSOLE_URL: &str = "https://www.kimi.com/code/console";
const WAIT_TIMEOUT: Duration = Duration::from_secs(90);
/// 进入终止态之后状态仍可回查的时长。从发布终止态起算，不从回调收下起算。
const STATUS_RETAIN: Duration = Duration::from_secs(10);
/// 回调已收下但一直没有 `publish_bind_outcome` 时的监听上限。
/// 必须盖过会员 `GetSubscription` 的 15 秒 `HTTP_TIMEOUT`；到点按不存在收场，不报等待超时。
const STATUS_UNPUBLISHED_CAP: Duration = Duration::from_secs(20);
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;

const STATUS_RESULT_PENDING: &str = "pending";
const STATUS_RESULT_SUCCESS: &str = "success";
const STATUS_RESULT_FAILURE: &str = "failure";
const STATUS_REASON_PENDING: &str = "等待中";
const STATUS_REASON_SUCCESS: &str = "绑定成功";
/// 「已取消」只给用户取消。
const STATUS_REASON_CANCELLED: &str = "已取消";
/// 「等待超时」只给等待超时。
const STATUS_REASON_TIMEOUT: &str = "等待超时";
/// 空 token、无法辨认网页身份、账号不存在、其余「绑定失败：…」、目标已变化：
/// 这次没有留下可用绑定。
const STATUS_REASON_NO_CREDENTIAL: &str = "无可用网页凭证";
/// 「账号身份不一致」只给身份门失败。
const STATUS_REASON_IDENTITY: &str = "账号身份不一致";

const STATUS_ABSENT: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

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

#[derive(Clone, Copy)]
enum StatusKind {
    /// 还没收到合法回调，或回调已收下但 bind_web 还没发布结果。
    Pending,
    Success,
    Failure(&'static str),
}

struct StatusSlot {
    kind: StatusKind,
    /// 终止态可见窗口终点。等待中为 `None`，不按 retain 过期。
    visible_until: Option<Instant>,
    /// 已收下但尚未发布结果时的硬截止。到点后回查变为不存在，而不是等待超时。
    unpublished_deadline: Option<Instant>,
}

struct StatusBook {
    slots: Mutex<HashMap<String, StatusSlot>>,
    retain: Duration,
    /// 可见窗口或未发布上限结束后，监听继续用与未知 nonce 相同的响应接客，然后关闭。
    /// 生产为 0；测试注入非零值，才能在同一端口上比较过期与伪造的字节。
    close_grace: Duration,
    /// 回调已收下、管道尚未发布时，监听最多再活这么久。生产 20 秒。
    unpublished_cap: Duration,
    /// 终止态发布后唤醒监听，把关闭时刻从「未发布上限」改到终止后的保留窗口。
    changed: watch::Sender<u64>,
}

impl StatusBook {
    fn production() -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            slots: Mutex::new(HashMap::new()),
            retain: STATUS_RETAIN,
            close_grace: Duration::ZERO,
            unpublished_cap: STATUS_UNPUBLISHED_CAP,
            changed,
        }
    }

    fn insert_pending(&self, nonce: &str) {
        self.slots.lock().expect("web bind status lock").insert(
            nonce.to_string(),
            StatusSlot {
                kind: StatusKind::Pending,
                visible_until: None,
                unpublished_deadline: None,
            },
        );
    }

    fn notify(&self) {
        self.changed.send_modify(|tick| *tick = tick.wrapping_add(1));
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// 只从「等待中」进入终止态，并把保留窗口从这一刻起算。
    /// 已终止的不再改写。未发布上限已过的按不存在留下，不补记终止态。
    fn finish(&self, nonce: &str, kind: StatusKind) {
        let applied = {
            let mut slots = self.slots.lock().expect("web bind status lock");
            let Some(slot) = slots.get_mut(nonce) else {
                return;
            };
            if !matches!(slot.kind, StatusKind::Pending) {
                return;
            }
            if slot
                .unpublished_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                return;
            }
            slot.kind = kind;
            slot.unpublished_deadline = None;
            slot.visible_until = Some(Instant::now() + self.retain);
            true
        };
        if applied {
            self.notify();
        }
    }

    /// 合法回调已收下，管道尚未发布：保持等待中。
    /// 这段不按 retain 过期；若调用方一直不回来，则到未发布上限后收场。
    fn arm_accepted(&self, nonce: &str) {
        let mut slots = self.slots.lock().expect("web bind status lock");
        let Some(slot) = slots.get_mut(nonce) else {
            return;
        };
        if matches!(slot.kind, StatusKind::Pending) && slot.unpublished_deadline.is_none() {
            slot.unpublished_deadline = Some(Instant::now() + self.unpublished_cap);
        }
    }

    fn view(&self, nonce: &str) -> Option<(&'static str, &'static str)> {
        let slots = self.slots.lock().expect("web bind status lock");
        let slot = slots.get(nonce)?;
        let now = Instant::now();
        // 等待中（含已收下未发布）不按 retain 过期。未发布上限到点后是不存在，不是等待超时。
        let expired = match slot.kind {
            StatusKind::Pending => slot
                .unpublished_deadline
                .is_some_and(|deadline| now >= deadline),
            _ => slot.visible_until.is_some_and(|until| now >= until),
        };
        if expired {
            return None;
        }
        Some(match slot.kind {
            StatusKind::Pending => (STATUS_RESULT_PENDING, STATUS_REASON_PENDING),
            StatusKind::Success => (STATUS_RESULT_SUCCESS, STATUS_REASON_SUCCESS),
            StatusKind::Failure(reason) => (STATUS_RESULT_FAILURE, reason),
        })
    }

    /// 监听还应开到何时。`None` 表示会话已收场，可以关端口。
    fn listen_until(&self, nonce: &str) -> Option<Instant> {
        let slots = self.slots.lock().expect("web bind status lock");
        let slot = slots.get(nonce)?;
        let base = match slot.kind {
            StatusKind::Pending => slot.unpublished_deadline?,
            _ => slot.visible_until?,
        };
        Some(base + self.close_grace)
    }

    fn on_window_elapsed(&self, nonce: &str) {
        let mut slots = self.slots.lock().expect("web bind status lock");
        let drop_slot = match slots.get(nonce) {
            None => false,
            Some(slot) => {
                let now = Instant::now();
                let base = match slot.kind {
                    StatusKind::Pending => slot.unpublished_deadline,
                    _ => slot.visible_until,
                };
                match base {
                    None => true,
                    Some(deadline) => now >= deadline + self.close_grace,
                }
            }
        };
        if drop_slot {
            slots.remove(nonce);
        }
    }

    fn remove(&self, nonce: &str) {
        self.slots.lock().expect("web bind status lock").remove(nonce);
    }
}

pub struct WebBindBroker {
    pending: Mutex<HashMap<String, Arc<Pending>>>,
    status: Arc<StatusBook>,
    /// 会话时长；生产固定 90s，测试注入更短值以覆盖超时路径。
    wait_timeout: Duration,
}

impl Default for WebBindBroker {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            status: Arc::new(StatusBook::production()),
            wait_timeout: WAIT_TIMEOUT,
        }
    }
}

impl WebBindBroker {
    #[cfg(test)]
    fn for_test(
        wait: Duration,
        retain: Duration,
        grace: Duration,
        unpublished_cap: Duration,
    ) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            pending: Mutex::new(HashMap::new()),
            status: Arc::new(StatusBook {
                slots: Mutex::new(HashMap::new()),
                retain,
                close_grace: grace,
                unpublished_cap,
                changed,
            }),
            wait_timeout: wait,
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
        self.status.insert_pending(&nonce);

        let task_nonce = nonce.clone();
        let timeout = self.wait_timeout;
        let status = Arc::clone(&self.status);
        tokio::spawn(async move {
            serve_session(listener, task_nonce, cancel_rx, timeout, status, sender).await;
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
            biased;
            _ = wait_cancelled(&mut cancel_rx) => Err(anyhow!("Edge 绑定已取消")),
            value = tokio::time::timeout(self.wait_timeout, receiver) => match value {
                Err(_) => Err(anyhow!("Edge 绑定等待超时")),
                Ok(Err(_)) => Err(anyhow!("Edge 绑定回调已断开")),
                Ok(Ok(result)) => result,
            },
        };
        self.pending
            .lock()
            .expect("web bind pending lock")
            .remove(nonce);
        let token = match delivered {
            Ok(token) => token,
            Err(error) => {
                self.note_wait_error(nonce, &error);
                return Err(error);
            }
        };
        if pending.target != expected_target {
            // 凭证丢弃，管道未跑。这次没有留下可用绑定。
            self.status
                .finish(nonce, StatusKind::Failure(STATUS_REASON_NO_CREDENTIAL));
            return Err(anyhow!("Edge 绑定目标已变化，请重试"));
        }
        Ok(token)
    }

    /// 把 bind_web 的结局写进状态回查。`None` 表示管道返回 Ok。
    /// `Some` 是返回给 GUI 的既有中文文案，这里只读映射，不改那句文案。
    pub(crate) fn publish_bind_outcome(&self, nonce: &str, gui_message: Option<&str>) {
        let kind = match gui_message {
            None => StatusKind::Success,
            Some(message) => map_bind_gui_message(message),
        };
        self.status.finish(nonce, kind);
    }

    fn note_wait_error(&self, nonce: &str, error: &anyhow::Error) {
        let reason = match error.to_string().as_str() {
            "Edge 绑定已取消" => STATUS_REASON_CANCELLED,
            "Edge 绑定等待超时" => STATUS_REASON_TIMEOUT,
            // 空 token、回调断开：这次没有留下可用绑定。
            _ => STATUS_REASON_NO_CREDENTIAL,
        };
        self.status.finish(nonce, StatusKind::Failure(reason));
    }

    /// 作废一个等待中的绑定会话并关闭回调监听；返回该会话是否仍存在。
    pub fn cancel(&self, nonce: &str) -> bool {
        let pending = self
            .pending
            .lock()
            .expect("web bind pending lock")
            .remove(nonce);
        if let Some(pending) = pending {
            // 「已取消」只走这条取消路径。
            self.status
                .finish(nonce, StatusKind::Failure(STATUS_REASON_CANCELLED));
            let _ = pending.cancel_tx.send(true);
            true
        } else {
            false
        }
    }
}

/// 身份门失败只认 GUI 那一句；会员读取失败（凭证已写入）仍是成功。
/// 无法辨认、账号不存在、其余「绑定失败：…」合成「无可用网页凭证」：
/// 这次没有留下可用绑定。
fn map_bind_gui_message(message: &str) -> StatusKind {
    match message {
        "网页身份与该账号不一致，未绑定" => StatusKind::Failure(STATUS_REASON_IDENTITY),
        "会员信息获取失败，请稍后再试" => StatusKind::Success,
        _ => StatusKind::Failure(STATUS_REASON_NO_CREDENTIAL),
    }
}

fn random_nonce() -> Result<String> {
    let mut bytes = [0_u8; 24];
    rand::rng()
        .try_fill_bytes(&mut bytes)
        .map_err(|error| anyhow!("无法生成 Edge 绑定随机数：{error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn serve_session(
    listener: TcpListener,
    nonce: String,
    mut cancelled: watch::Receiver<bool>,
    timeout: Duration,
    status: Arc<StatusBook>,
    sender: oneshot::Sender<Result<String>>,
) {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let delivery = loop {
        let (stream, _) = tokio::select! {
            biased;
            _ = wait_cancelled(&mut cancelled) => {
                status.finish(&nonce, StatusKind::Failure(STATUS_REASON_CANCELLED));
                break Err(anyhow!("Edge 绑定已取消"));
            }
            _ = &mut deadline => {
                status.finish(&nonce, StatusKind::Failure(STATUS_REASON_TIMEOUT));
                break Err(anyhow!("Edge 绑定等待超时"));
            }
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(error) => {
                    break Err(anyhow::Error::from(error).context("Edge 绑定回调连接失败"));
                }
            },
        };
        // 会话剩余预算约束整条连接的处理：静默连接（建连不发数据）或声明
        // Content-Length 后少发正文，都不得把任务拖过截止时间（F3）。
        let budget = deadline
            .deadline()
            .saturating_duration_since(tokio::time::Instant::now());
        let handled = tokio::time::timeout(
            budget,
            handle_connection(stream, &nonce, true, Some(status.as_ref())),
        )
        .await;
        match handled {
            Err(_) => {
                status.finish(&nonce, StatusKind::Failure(STATUS_REASON_TIMEOUT));
                break Err(anyhow!("Edge 绑定等待超时"));
            }
            Ok(Ok(token)) => {
                // 先标成「已收下、管道未结束」（仍是等待中，不开始终止后的保留窗口），
                // 再把 token 交给 wait。
                status.arm_accepted(&nonce);
                break Ok(token);
            }
            // 合法回调但无法完成绑定（页面没有可用凭证）：终止会话，错误直达等待方（F1）。
            // 这次没有留下可用绑定。
            Ok(Err(CallbackError::Terminated(error))) => {
                status.finish(&nonce, StatusKind::Failure(STATUS_REASON_NO_CREDENTIAL));
                break Err(error);
            }
            // 无效请求（方法/路径/nonce/大小/格式/连接中断）：忽略并继续等待。
            Ok(Err(CallbackError::Ignored(error))) => {
                tracing::debug!(error = %error, "ignored invalid Edge web bind callback");
            }
            // 状态回查已写完响应：无副作用，继续等回调。
            Ok(Err(CallbackError::StatusWritten)) => {}
        }
    };
    let _ = sender.send(delivery);
    let changed = status.subscribe();
    serve_status_window(&listener, &nonce, status.as_ref(), changed).await;
    status.remove(&nonce);
}

/// 回调收下后的只读窗口：发布前一直可 GET「等待中」，直到未发布上限；
/// 进入终止态后再保留一段，然后与未知 nonce 一样不再回答，并关闭监听。
/// POST 不再 204，也不改已终止状态。
async fn serve_status_window(
    listener: &TcpListener,
    nonce: &str,
    status: &StatusBook,
    mut changed: watch::Receiver<u64>,
) {
    loop {
        let Some(deadline) = status.listen_until(nonce) else {
            break;
        };
        let rest = deadline.saturating_duration_since(Instant::now());
        if rest.is_zero() {
            status.on_window_elapsed(nonce);
            continue;
        }
        tokio::select! {
            biased;
            _ = changed.changed() => {}
            _ = tokio::time::sleep(rest) => status.on_window_elapsed(nonce),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let budget = deadline.saturating_duration_since(Instant::now());
                    if budget.is_zero() {
                        status.on_window_elapsed(nonce);
                        continue;
                    }
                    let _ = tokio::time::timeout(
                        budget,
                        handle_connection(stream, nonce, false, Some(status)),
                    )
                    .await;
                }
                Err(_) => break,
            },
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
    /// 状态响应已写回；调用方继续监听，不把它当成回调失败。
    StatusWritten,
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
#[cfg(test)]
async fn handle_callback<S>(stream: S, expected_nonce: &str) -> Result<String, CallbackError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    handle_connection(stream, expected_nonce, true, None).await
}

/// `deliver_token` 为假时处于回查窗口：可以回答 GET 状态，但不再交付、不再回 204。
async fn handle_connection<S>(
    stream: S,
    expected_nonce: &str,
    deliver_token: bool,
    status: Option<&StatusBook>,
) -> Result<String, CallbackError>
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
    if request.len() < 2 {
        return Err(anyhow!("非法 Edge 回调路径").into());
    }
    if request[0] == "GET" {
        if let Some(query_nonce) = parse_status_nonce(request[1]) {
            if let Some(book) = status {
                let view = if query_nonce == expected_nonce {
                    book.view(query_nonce)
                } else {
                    None
                };
                write_status_response(reader.into_inner(), view).await?;
                return Err(CallbackError::StatusWritten);
            }
        }
        return Err(anyhow!("非法 Edge 回调路径").into());
    }
    // 回查窗口里的 POST 不再交付，也不回 204，避免改掉已终止状态。
    if !deliver_token {
        return Err(anyhow!("Edge 绑定回调已结束").into());
    }
    if request[0] != "POST" || request[1] != CALLBACK_PATH {
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

fn parse_status_nonce(target: &str) -> Option<&str> {
    let (path, query) = target.split_once('?')?;
    if path != STATUS_PATH {
        return None;
    }
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("nonce=") {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

#[derive(Serialize)]
struct WebBindStatusBody<'a> {
    result: &'a str,
    reason: &'a str,
}

async fn write_status_response<S>(
    mut stream: S,
    view: Option<(&str, &str)>,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let bytes = match view {
        Some((result, reason)) => {
            let payload = serde_json::to_vec(&WebBindStatusBody { result, reason })
                .expect("状态载荷只含两个字符串");
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let mut out = header.into_bytes();
            out.extend(payload);
            out
        }
        None => STATUS_ABSENT.to_vec(),
    };
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;
    Ok(())
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
    /// 状态保留窗口为 0，既有用例仍能在预算结束后马上看到监听关闭。
    fn test_broker(timeout: Duration) -> WebBindBroker {
        WebBindBroker::for_test(
            timeout,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        )
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
                CallbackError::StatusWritten => panic!("{label}：这些用例不应写成状态响应"),
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

    /// 状态回查路径是独立字面量，不能和 POST 回调路径撞车。
    #[test]
    fn status_path_is_distinct_from_callback() {
        assert_eq!(STATUS_PATH, "/kimi-switch/web-bind-status");
        assert_ne!(STATUS_PATH, CALLBACK_PATH);
    }

    fn status_broker(wait: Duration, retain: Duration, grace: Duration) -> WebBindBroker {
        // 未发布上限留得比这些用例长，避免和「终止后的短保留」缠在一起。
        WebBindBroker::for_test(wait, retain, grace, Duration::from_secs(30))
    }

    fn status_request(nonce: &str) -> Vec<u8> {
        format!(
            "GET /kimi-switch/web-bind-status?nonce={nonce} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    async fn get_status(port: u16, nonce: &str) -> String {
        let mut last = String::new();
        for _ in 0..25 {
            last = send_raw(port, status_request(nonce)).await;
            if !last.is_empty() {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        last
    }

    fn status_body(response: &str) -> &str {
        response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("")
    }

    /// 载荷只有结果枚举和一句中文原因；期望值是规格字面量，不读实现常量。
    fn assert_public_status(response: &str, result: &str, reason: &str) {
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "状态应可回查：{response}"
        );
        let value: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(status_body(response)).unwrap_or_else(|error| {
                panic!("状态载荷不是 JSON 对象：{error}；{response}");
            });
        assert_eq!(value.len(), 2, "载荷只能有结果与原因：{response}");
        assert_eq!(
            value.get("result").and_then(|item| item.as_str()),
            Some(result),
            "{response}"
        );
        assert_eq!(
            value.get("reason").and_then(|item| item.as_str()),
            Some(reason),
            "{response}"
        );
    }

    fn assert_no_secrets(response: &str) {
        assert!(!response.contains(TEST_TOKEN), "载荷泄漏凭证：{response}");
        assert!(!response.contains(INTRUDER_TOKEN), "载荷泄漏凭证：{response}");
        assert!(!response.contains("alice"), "载荷泄漏账号 id：{response}");
        assert!(!response.contains("bob"), "载荷泄漏账号 id：{response}");
        assert!(!response.contains("eyJ"), "载荷泄漏 JWT：{response}");
    }

    fn assert_absent(response: &str) {
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "未知或过期应不存在：{response}"
        );
        assert!(status_body(response).is_empty(), "不存在响应不得带载荷：{response}");
        assert_no_secrets(response);
    }

    /// 还没收到合法回调：等待中。回查无副作用，随后的合法回调仍只交付一次。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_before_callback_is_pending_and_has_no_side_effect() {
        let broker = status_broker(Duration::from_secs(5), Duration::from_secs(3), Duration::from_secs(1));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);

        let first = get_status(port, &challenge.nonce).await;
        assert_public_status(&first, "pending", "等待中");
        assert_no_secrets(&first);
        assert!(!first.contains(&challenge.nonce), "载荷不得回显 nonce：{first}");
        let second = get_status(port, &challenge.nonce).await;
        assert_eq!(first, second, "重复回查不得改变等待中");

        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        let token = broker.wait(&challenge.nonce, "alice").await.unwrap();
        assert_eq!(token, TEST_TOKEN);
        assert!(send.await.unwrap().starts_with("HTTP/1.1 204"));
    }

    /// 回调已收下、bind_web 还没返回：仍然是等待中。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_after_accept_before_bind_is_pending() {
        let broker = status_broker(Duration::from_secs(5), Duration::from_secs(3), Duration::from_secs(1));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        assert_eq!(
            broker.wait(&challenge.nonce, "alice").await.unwrap(),
            TEST_TOKEN
        );
        send.await.unwrap();

        let response = get_status(port, &challenge.nonce).await;
        assert_public_status(&response, "pending", "等待中");
        assert_no_secrets(&response);
        assert!(broker.wait(&challenge.nonce, "alice").await.is_err());
    }

    /// 用户取消 → 失败：已取消。等待方的错误文案仍是原来的取消句。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_user_cancel_is_cancelled() {
        let broker = Arc::new(status_broker(
            Duration::from_secs(5),
            Duration::from_secs(3),
            Duration::from_secs(1),
        ));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let waiter = Arc::clone(&broker);
        let nonce = challenge.nonce.clone();
        let wait_handle = tokio::spawn(async move { waiter.wait(&nonce, "alice").await });
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(broker.cancel(&challenge.nonce));

        let error = wait_handle.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("取消"), "{error}");
        let response = get_status(port, &challenge.nonce).await;
        assert_public_status(&response, "failure", "已取消");
        assert_no_secrets(&response);
        assert!(!response.contains("等待超时"));
        assert!(!response.contains("账号身份不一致"));
    }

    /// 等待超时 → 失败：等待超时。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_wait_timeout_is_timeout() {
        let broker = status_broker(
            Duration::from_millis(250),
            Duration::from_secs(3),
            Duration::from_secs(1),
        );
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let error = broker.wait(&challenge.nonce, "alice").await.unwrap_err();
        assert!(error.to_string().contains("超时"), "{error}");
        let response = get_status(port, &challenge.nonce).await;
        assert_public_status(&response, "failure", "等待超时");
        assert_no_secrets(&response);
        assert!(!response.contains("已取消"));
    }

    /// 空 token → 失败：无可用网页凭证。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_empty_token_is_no_credential() {
        let broker = status_broker(Duration::from_secs(5), Duration::from_secs(3), Duration::from_secs(1));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, "   ")));
        let error = tokio::time::timeout(Duration::from_secs(2), broker.wait(&challenge.nonce, "alice"))
            .await
            .expect("空 token 必须立即失败")
            .unwrap_err();
        assert!(error.to_string().contains("没有可用网页凭证"), "{error}");
        send.await.unwrap();
        let response = get_status(port, &challenge.nonce).await;
        assert_public_status(&response, "failure", "无可用网页凭证");
        assert_no_secrets(&response);
    }

    /// bind_web 的成功/失败按已裁定的 GUI 文案映射；会员读取失败仍是成功。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_bind_outcomes_follow_gui_messages() {
        let cases: &[(&str, Option<&str>, &str, &str)] = &[
            ("bind_web 返回 Ok", None, "success", "绑定成功"),
            (
                "会员信息获取失败，请稍后再试",
                Some("会员信息获取失败，请稍后再试"),
                "success",
                "绑定成功",
            ),
            (
                "网页身份与该账号不一致，未绑定",
                Some("网页身份与该账号不一致，未绑定"),
                "failure",
                "账号身份不一致",
            ),
            (
                "无法从该 token 辨认网页身份，未绑定",
                Some("无法从该 token 辨认网页身份，未绑定"),
                "failure",
                "无可用网页凭证",
            ),
            (
                "账号不存在，请刷新后再试",
                Some("账号不存在，请刷新后再试"),
                "failure",
                "无可用网页凭证",
            ),
            (
                "绑定失败：库存写入失败",
                Some("绑定失败：库存写入失败"),
                "failure",
                "无可用网页凭证",
            ),
        ];
        for (label, message, result, reason) in cases {
            let broker = status_broker(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(1),
            );
            let challenge = broker.begin("alice").await.unwrap();
            let port = challenge_port(&challenge);
            let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
            assert_eq!(
                broker.wait(&challenge.nonce, "alice").await.unwrap(),
                TEST_TOKEN,
                "{label}"
            );
            send.await.unwrap();
            let pending = get_status(port, &challenge.nonce).await;
            assert_public_status(&pending, "pending", "等待中");

            broker.publish_bind_outcome(&challenge.nonce, *message);
            let response = get_status(port, &challenge.nonce).await;
            assert_public_status(&response, result, reason);
            assert_no_secrets(&response);
            assert!(
                !response.contains("会员信息获取失败"),
                "{label}：会员失败细节不得进回查载荷：{response}"
            );
            assert!(
                !response.contains("网页身份与该账号不一致"),
                "{label}：身份门长文案不得进回查载荷：{response}"
            );
            assert!(!response.contains(TEST_TOKEN), "{label}");
        }
    }

    /// 目标已变化：凭证丢弃，管道未跑，回查为无可用网页凭证。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_target_changed_is_no_credential() {
        let broker = status_broker(Duration::from_secs(5), Duration::from_secs(3), Duration::from_secs(1));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        let error = broker.wait(&challenge.nonce, "bob").await.unwrap_err();
        assert!(error.to_string().contains("目标已变化"), "{error}");
        assert!(!error.to_string().contains(TEST_TOKEN));
        send.await.unwrap();
        let response = get_status(port, &challenge.nonce).await;
        assert_public_status(&response, "failure", "无可用网页凭证");
        assert_no_secrets(&response);
        assert!(!response.contains("目标已变化"), "{response}");
    }

    /// 可见窗口结束后，真实 nonce 与伪造 nonce 的响应字节相同，且都不是成功载荷。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_after_retain_matches_unknown_nonce() {
        let broker = status_broker(
            Duration::from_secs(5),
            Duration::from_millis(200),
            Duration::from_secs(2),
        );
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        broker.wait(&challenge.nonce, "alice").await.unwrap();
        send.await.unwrap();
        broker.publish_bind_outcome(&challenge.nonce, None);
        let live = get_status(port, &challenge.nonce).await;
        assert_public_status(&live, "success", "绑定成功");

        tokio::time::sleep(Duration::from_millis(400)).await;
        let expired = send_raw(port, status_request(&challenge.nonce)).await;
        let forged = send_raw(port, status_request(&"cd".repeat(24))).await;
        assert_eq!(expired, forged, "过期后必须与伪造 nonce 不可区分");
        assert_absent(&expired);
        assert!(!expired.contains("绑定成功"));
        assert!(!expired.contains(&challenge.nonce));
    }

    /// 伪造 nonce 看不到真实会话的结果、凭证或账号。
    #[tokio::test(flavor = "multi_thread")]
    async fn forged_nonce_does_not_leak_session() {
        let broker = status_broker(Duration::from_secs(5), Duration::from_secs(3), Duration::from_secs(1));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        broker.wait(&challenge.nonce, "alice").await.unwrap();
        send.await.unwrap();
        broker.publish_bind_outcome(&challenge.nonce, None);
        let real = get_status(port, &challenge.nonce).await;
        assert_public_status(&real, "success", "绑定成功");

        let forged = get_status(port, &"ab".repeat(24)).await;
        let other = get_status(port, "nope").await;
        assert_eq!(forged, other, "不同的伪造 nonce 必须同一份不存在响应");
        assert_ne!(forged, real);
        assert_absent(&forged);
        assert!(!forged.contains("绑定成功"));
        assert!(!forged.contains("等待中"));
        assert!(!forged.contains(&challenge.nonce));
        let again = get_status(port, &challenge.nonce).await;
        assert_eq!(real, again, "伪造回查不得改变已发布状态");
    }

    /// 成功载荷也不含凭证、账号 id、nonce。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_payload_omits_credentials() {
        let broker = status_broker(Duration::from_secs(5), Duration::from_secs(3), Duration::from_secs(1));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        broker.wait(&challenge.nonce, "alice").await.unwrap();
        send.await.unwrap();
        broker.publish_bind_outcome(&challenge.nonce, None);
        let response = get_status(port, &challenge.nonce).await;
        assert_public_status(&response, "success", "绑定成功");
        assert_no_secrets(&response);
        assert!(!status_body(&response).contains(&challenge.nonce));
        assert!(!response.contains("网页身份"));
    }

    /// 重复回调不改已终止状态，也不再交付 token。
    #[tokio::test(flavor = "multi_thread")]
    async fn repeat_callback_does_not_change_terminal_status() {
        let broker = status_broker(Duration::from_secs(5), Duration::from_secs(3), Duration::from_secs(1));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        assert_eq!(
            broker.wait(&challenge.nonce, "alice").await.unwrap(),
            TEST_TOKEN
        );
        send.await.unwrap();
        broker.publish_bind_outcome(&challenge.nonce, None);
        let first = get_status(port, &challenge.nonce).await;
        assert_public_status(&first, "success", "绑定成功");

        let repeat = send_raw(port, post_request(&challenge.nonce, INTRUDER_TOKEN)).await;
        assert!(!repeat.contains("204"), "重复回调不得再被消费：{repeat}");
        broker.publish_bind_outcome(
            &challenge.nonce,
            Some("网页身份与该账号不一致，未绑定"),
        );
        let second = get_status(port, &challenge.nonce).await;
        assert_eq!(first, second, "重复回调和再次发布不得改变已终止状态");
        assert_no_secrets(&second);
        assert!(broker.wait(&challenge.nonce, "alice").await.is_err());
    }

    /// 管道结果晚于旧的「收下即 10 秒」窗口才发布：回查仍要看到终止态，
    /// 终止后再保留一个短窗口，窗口结束与未知 nonce 一样 404。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_published_after_accept_window_stays_readable_then_absent() {
        let retain = Duration::from_millis(200);
        let broker = status_broker(Duration::from_secs(5), retain, Duration::from_secs(3));
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        broker.wait(&challenge.nonce, "alice").await.unwrap();
        send.await.unwrap();

        // 旧实现从收下起算 retain，这段睡眠已经越过那个窗口。
        tokio::time::sleep(Duration::from_millis(450)).await;
        let pending = get_status(port, &challenge.nonce).await;
        assert_public_status(&pending, "pending", "等待中");
        assert_no_secrets(&pending);

        broker.publish_bind_outcome(&challenge.nonce, None);
        let terminal = get_status(port, &challenge.nonce).await;
        assert_public_status(&terminal, "success", "绑定成功");
        let still = get_status(port, &challenge.nonce).await;
        assert_eq!(terminal, still, "终止态在保留窗口内应保持可读");

        tokio::time::sleep(Duration::from_millis(450)).await;
        let expired = send_raw(port, status_request(&challenge.nonce)).await;
        let forged = send_raw(port, status_request(&"cd".repeat(24))).await;
        assert_eq!(expired, forged, "终止窗口结束后必须与伪造 nonce 不可区分");
        assert_absent(&expired);
        assert!(!expired.contains("绑定成功"));
        assert!(!expired.contains("等待超时"));
    }

    /// `complete_web_bind` 用 `{:#}` 传文案。单层 anyhow 必须仍精确命中这两句。
    #[tokio::test(flavor = "multi_thread")]
    async fn status_maps_anyhow_alternate_display_without_gui() {
        let cases = [
            (
                "网页身份与该账号不一致，未绑定",
                "failure",
                "账号身份不一致",
            ),
            ("会员信息获取失败，请稍后再试", "success", "绑定成功"),
        ];
        for (sentence, result, reason) in cases {
            let broker = status_broker(
                Duration::from_secs(5),
                Duration::from_secs(3),
                Duration::from_secs(1),
            );
            let challenge = broker.begin("alice").await.unwrap();
            let port = challenge_port(&challenge);
            let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
            broker.wait(&challenge.nonce, "alice").await.unwrap();
            send.await.unwrap();
            let error = anyhow::anyhow!(sentence);
            let message = format!("{error:#}");
            assert_eq!(message, sentence, "{{:#}} 必须就是这句 GUI 文案");
            broker.publish_bind_outcome(&challenge.nonce, Some(&message));
            let response = get_status(port, &challenge.nonce).await;
            assert_public_status(&response, result, reason);
            assert_no_secrets(&response);
            assert!(!response.contains("会员信息获取失败"), "{response}");
        }
    }

    /// 回调已收下但一直不发布：到未发布上限后是不存在，不是等待超时，补发布也不能复活。
    #[tokio::test(flavor = "multi_thread")]
    async fn unpublished_callback_expires_as_absent_not_timeout() {
        let broker = WebBindBroker::for_test(
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(2),
            Duration::from_millis(200),
        );
        let challenge = broker.begin("alice").await.unwrap();
        let port = challenge_port(&challenge);
        let send = tokio::spawn(send_raw(port, post_request(&challenge.nonce, TEST_TOKEN)));
        broker.wait(&challenge.nonce, "alice").await.unwrap();
        send.await.unwrap();

        let pending = get_status(port, &challenge.nonce).await;
        assert_public_status(&pending, "pending", "等待中");

        tokio::time::sleep(Duration::from_millis(450)).await;
        let expired = send_raw(port, status_request(&challenge.nonce)).await;
        let forged = send_raw(port, status_request(&"ab".repeat(24))).await;
        assert_eq!(expired, forged, "未发布上限过后必须与伪造 nonce 不可区分");
        assert_absent(&expired);
        assert!(!expired.contains("等待超时"), "{expired}");
        assert!(!expired.contains("等待中"), "{expired}");

        broker.publish_bind_outcome(&challenge.nonce, None);
        let revived = send_raw(port, status_request(&challenge.nonce)).await;
        assert_eq!(revived, expired, "上限之后发布不得把会话复活成成功");
        assert!(!revived.contains("绑定成功"));
    }
}
