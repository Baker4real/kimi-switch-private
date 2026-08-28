//! GUI 命令层：给 Tauri webview 提供不含凭证的展示 DTO。
//!
//! webview 不得接触 access_token / refresh_token / 凭证 blob。

mod backend;
mod dto;

pub use backend::Backend;
pub use dto::{
    AccountRow, AuthLink, QuotaView, Snapshot, ThemeId, UiMessage, URGENT_USED_RATIO,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_tone_follows_pressure_thresholds() {
        assert_eq!(QuotaView::tone(Some(0.0)), "ok");
        assert_eq!(QuotaView::tone(Some(0.49)), "ok");
        assert_eq!(QuotaView::tone(Some(0.5)), "warn");
        assert_eq!(QuotaView::tone(Some(0.79)), "warn");
        assert_eq!(QuotaView::tone(Some(0.8)), "high");
        assert_eq!(QuotaView::tone(Some(0.94)), "high");
        assert_eq!(QuotaView::tone(Some(0.95)), "full");
        assert_eq!(QuotaView::tone(None), "ok");
    }

    #[test]
    fn urgent_when_used_at_least_80_percent_or_error() {
        let quiet = AccountRow {
            id: "a".into(),
            label: "a".into(),
            active: false,
            membership: None,
            quotas: vec![QuotaView {
                window: "5小时".into(),
                ratio: Some(0.79),
                text: "79%".into(),
                reset: None,
                tone: "warn".into(),
            }],
            error: None,
        };
        assert!(!quiet.is_urgent());

        let hot = AccountRow {
            quotas: vec![QuotaView {
                window: "7天".into(),
                ratio: Some(0.8),
                text: "80%".into(),
                reset: None,
                tone: "high".into(),
            }],
            ..quiet.clone()
        };
        assert!(hot.is_urgent());

        let failed = AccountRow {
            quotas: vec![],
            error: Some("额度查询失败".into()),
            ..quiet
        };
        assert!(failed.is_urgent());
    }

    #[test]
    fn theme_id_only_classic_or_jade_night() {
        assert_eq!(ThemeId::parse("classic"), ThemeId::Classic);
        assert_eq!(ThemeId::parse("jade-night"), ThemeId::JadeNight);
        assert_eq!(ThemeId::parse("mica"), ThemeId::Classic);
        assert_eq!(ThemeId::parse(""), ThemeId::Classic);
    }
}
