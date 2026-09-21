chrome.runtime.onMessage.addListener((message) => {
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
  // 固定 loopback 目标；使用 text/plain + no-cors 避免向本机 listener 引入预检状态机。
  void fetch(`http://127.0.0.1:${port}/kimi-switch/web-bind`, {
    method: "POST",
    mode: "no-cors",
    headers: { "Content-Type": "text/plain" },
    body: JSON.stringify({ nonce, token }),
  }).catch(() => undefined);
});
