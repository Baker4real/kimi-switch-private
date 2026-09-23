const STATUS_POLL_INTERVAL_MS = 2000;
const STATUS_POLL_MAX_ATTEMPTS = 12;
const STATUS_PROBE_INTERVAL_MS = 250;
const STATUS_PROBE_MAX_ATTEMPTS = 3;

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || message.type !== "kimi-switch-web-bind") {
    return;
  }
  const { nonce, port, token } = message;
  if (
    typeof nonce !== "string" ||
    !/^[0-9a-f]{48}$/.test(nonce) ||
    !Number.isInteger(port) ||
    port < 1024 ||
    port > 65535 ||
    typeof token !== "string"
  ) {
    return;
  }
  // 保持消息通道，等有限次回查结束后把结果交回挑战页。
  void postAcceptedThenPoll(port, nonce, token)
    .then((outcome) => {
      sendResponse(outcome ?? null);
    })
    .catch(() => undefined);
  return true;
});

async function postAcceptedThenPoll(port, nonce, token) {
  // 204 之后才做 22 秒轮询。抛错或非 204 只短探测已有终止态，避免把仍在等待的会话说成超时。
  let posted;
  try {
    // 扩展已有 127.0.0.1 host permission，能直接读 loopback 响应；
    // text/plain 仍是简单请求，不向 listener 引入预检。
    posted = await fetch(`http://127.0.0.1:${port}/kimi-switch/web-bind`, {
      method: "POST",
      headers: { "Content-Type": "text/plain" },
      body: JSON.stringify({ nonce, token }),
    });
  } catch {
    return probeTerminalStatus(port, nonce);
  }
  if (!posted || posted.status !== 204) {
    return probeTerminalStatus(port, nonce);
  }
  return pollBindStatus(port, nonce);
}

async function pollBindStatus(port, nonce) {
  // 第一次立即查，之后每 2 秒一次，12 次的间隔跨度 22 秒，盖住收下后最多 20 秒的「等待中」。
  for (let attempt = 0; attempt < STATUS_POLL_MAX_ATTEMPTS; attempt += 1) {
    if (attempt > 0) {
      await delay(STATUS_POLL_INTERVAL_MS);
    }
    const viewed = await readBindStatus(port, nonce);
    const result = viewed ? viewed.result : "";
    const reason = viewed ? viewed.reason : "";
    if (result === "success" || result === "failure") {
      return publicOutcome(result, reason);
    }
    if (result === "pending") {
      continue;
    }
  }
  return { result: "failure", reason: "等待超时" };
}

async function probeTerminalStatus(port, nonce) {
  // 只确认服务端是否已经写下终止态。pending、404、连不上都不当作失败文案。
  for (let attempt = 0; attempt < STATUS_PROBE_MAX_ATTEMPTS; attempt += 1) {
    if (attempt > 0) {
      await delay(STATUS_PROBE_INTERVAL_MS);
    }
    const viewed = await readBindStatus(port, nonce);
    const result = viewed ? viewed.result : "";
    const reason = viewed ? viewed.reason : "";
    if (result === "success" || result === "failure") {
      return publicOutcome(result, reason);
    }
  }
  return null;
}

async function readBindStatus(port, nonce) {
  try {
    const response = await fetch(
      `http://127.0.0.1:${port}/kimi-switch/web-bind-status?nonce=${nonce}`,
      { method: "GET", cache: "no-store" },
    );
    // 404 或非 200：这次查询没有结果，由上层决定是否再试。
    if (response.status !== 200) {
      return null;
    }
    const body = await response.json();
    if (!body || typeof body.result !== "string" || typeof body.reason !== "string") {
      return null;
    }
    return { result: body.result, reason: body.reason };
  } catch {
    return null;
  }
}

function publicOutcome(result, reason) {
  if (result === "success" && reason === "绑定成功") {
    return { result: "success", reason: "绑定成功" };
  }
  const failureReasons = ["已取消", "等待超时", "无可用网页凭证", "账号身份不一致"];
  if (result === "failure" && failureReasons.indexOf(reason) !== -1) {
    return { result: "failure", reason };
  }
  return null;
}

function delay(ms) {
  return new Promise((resolve) => {
    setTimeout(resolve, ms);
  });
}
