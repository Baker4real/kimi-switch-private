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

  const failureReasons = ["已取消", "等待超时", "无可用网页凭证", "账号身份不一致"];

  function toastText(result, reason) {
    if (result === "success" && reason === "绑定成功") {
      return "绑定成功，可回到 kimi-switch 查看";
    }
    if (result === "failure" && failureReasons.indexOf(reason) !== -1) {
      return "绑定失败：" + reason;
    }
    return "";
  }

  function showToast(text) {
    if (!text) {
      return;
    }
    const node = document.createElement("div");
    node.setAttribute("role", "status");
    node.setAttribute("aria-live", "polite");
    node.textContent = text;
    node.style.cssText =
      "position:fixed;z-index:2147483647;right:16px;bottom:16px;max-width:360px;padding:12px 16px;border-radius:8px;background:#f8fafc;color:#0f172a;font:14px/1.5 sans-serif;box-shadow:0 8px 24px rgba(15,23,42,.18)";
    document.documentElement.appendChild(node);
    setTimeout(() => node.remove(), 5000);
  }

  void chrome.runtime.sendMessage({ type: "kimi-switch-web-bind", nonce, port, token }, (response) => {
    if (chrome.runtime.lastError || !response) {
      return;
    }
    showToast(toastText(response.result, response.reason));
  });
})();
