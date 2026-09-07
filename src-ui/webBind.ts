/** 绑定网页登录（issue #51）的纯逻辑：控制台地址、token 规整、对话框文案。
 * token 的 DOM 生命周期在 main.ts：对话框任何关闭路径都清空输入，不落 store。 */

/** 绑定对话框「打开控制台」按钮的目标页：网页 token 即该页 localStorage 的
 * `access_token`（#50 / ADR-0004），打开它用户才能拿到 token；只经现有
 * `open_url` 能力跳系统浏览器，不读浏览器 cookie、不做内嵌抓取。 */
export const WEB_BIND_CONSOLE_URL = "https://www.kimi.com/code/console";

/** trim 后的网页 token；空白输入返回 null（不发起绑定）。
 * 与 CLI `normalize_web_token` 同口径：粘贴常带首尾空白与换行。 */
export function parseWebToken(raw: string): string | null {
  const trimmed = raw.trim();
  return trimmed.length === 0 ? null : trimmed;
}

/** 绑定对话框的说明文案：账号显示名帮用户核对绑的是哪张卡，
 * 粘贴来源（控制台 localStorage 的 access_token）写清楚。 */
export function webBindCopy(displayName: string): string {
  return `绑定「${displayName}」后这张卡可显示会员到期。打开 kimi.com 控制台并登录同一账号，在浏览器开发者工具（F12）→ Application → Local Storage → www.kimi.com 中复制 access_token 的值，粘贴到下面。`;
}
