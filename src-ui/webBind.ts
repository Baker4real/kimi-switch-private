/** 绑定网页登录（issue #51）的纯逻辑：控制台地址、token 规整、对话框文案。
 * 自动读取路径只把一次性挑战交给 Edge；网页凭证不进入 WebView。 */

/** 网页凭证的来源页（官方控制台）。自动路径由 Rust 追加一次性挑战 fragment 后交给
 * Edge；手动兜底的「打开控制台」按钮打开无 fragment 的原始地址——扩展页面脚本
 * 检测不到挑战时直接返回，不会读取任何凭证。 */
export const WEB_BIND_CONSOLE_URL = "https://www.kimi.com/code/console";

/** trim 后的网页 token；空白输入返回 null（不发起绑定）。
 * 与 CLI `normalize_web_token` 同口径：粘贴常带首尾空白与换行。 */
export function parseWebToken(raw: string): string | null {
  const trimmed = raw.trim();
  return trimmed.length === 0 ? null : trimmed;
}

/** 绑定对话框的说明文案：自动读取优先，手动粘贴作为浏览器扩展不可用时的兜底。 */
export function webBindCopy(displayName: string): string {
  return `绑定「${displayName}」后这张卡可显示会员到期。优先从 Edge 控制台自动读取；如果 Edge 扩展未安装或读取失败，仍可在下方粘贴网页 access_token。`;
}
