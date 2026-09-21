(() => {
  const prefix = "#kimi-switch-bind=v1.";
  if (!location.hash.startsWith(prefix)) {
    return;
  }

  const [nonce, portText] = location.hash.slice(prefix.length).split(".");
  const port = Number(portText);
  if (!/^[0-9a-f]{48}$/.test(nonce) || !Number.isInteger(port) || port < 1024 || port > 65535) {
    return;
  }

  // 未登录时仍发送空 token，由本机 listener 拒绝并给出中文失败；
  // token 只进入扩展内部消息，不写入页面、URL 或扩展 storage。
  const token = localStorage.getItem("access_token") || "";
  // 挑战一次性消费：读取后立即清除 fragment，nonce/端口不残留在地址栏与会话历史。
  history.replaceState(null, "", `${location.pathname}${location.search}`);
  void chrome.runtime.sendMessage({ type: "kimi-switch-web-bind", nonce, port, token });
})();
