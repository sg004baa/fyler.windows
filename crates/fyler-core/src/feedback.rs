//! 匿名フィードバックの送信契約を表す純粋型。

use std::fmt::Write as _;

/// フィードバック本文の最大文字数。
pub const MAX_BODY_CHARS: usize = 4000;

/// フィードバックの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    Impression,
    Request,
    Bug,
}

impl FeedbackKind {
    /// GUIに表示する日本語名。
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Impression => "感想",
            Self::Request => "要望",
            Self::Bug => "不具合",
        }
    }

    /// 送信スキーマで使う文字列。
    pub const fn as_schema_str(self) -> &'static str {
        match self {
            Self::Impression => "impression",
            Self::Request => "request",
            Self::Bug => "bug",
        }
    }

    /// 送信スキーマの文字列から変換する。
    pub fn from_schema_str(value: &str) -> Option<Self> {
        match value {
            "impression" => Some(Self::Impression),
            "request" => Some(Self::Request),
            "bug" => Some(Self::Bug),
            _ => None,
        }
    }
}

/// サーバーへ送る匿名フィードバック。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackPayload {
    pub kind: FeedbackKind,
    pub body: String,
    pub app_version: String,
    pub os: String,
    pub arch: String,
}

impl FeedbackPayload {
    /// v1送信スキーマのJSONへエンコードする。
    pub fn to_json(&self) -> String {
        format!(
            "{{\"schema_version\":1,\"kind\":\"{}\",\"body\":\"{}\",\"app_version\":\"{}\",\"os\":\"{}\",\"arch\":\"{}\"}}",
            self.kind.as_schema_str(),
            escape_json(&self.body),
            escape_json(&self.app_version),
            escape_json(&self.os),
            escape_json(&self.arch),
        )
    }
}

/// フィードバック本文の検証エラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FeedbackBodyError {
    #[error("本文を入力してください")]
    Empty,
    #[error("本文は{MAX_BODY_CHARS}文字以内で入力してください")]
    TooLong,
}

/// 本文が空白のみでなく、上限文字数以内かを検証する。
pub fn validate_body(body: &str) -> Result<(), FeedbackBodyError> {
    if body.trim().is_empty() {
        return Err(FeedbackBodyError::Empty);
    }
    if body.chars().count() > MAX_BODY_CHARS {
        return Err(FeedbackBodyError::TooLong);
    }
    Ok(())
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{00}'..='\u{1f}' => {
                write!(&mut escaped, "\\u{:04X}", character as u32)
                    .expect("writing to String cannot fail");
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_schema_round_trip_and_display_names() {
        for (kind, schema, display) in [
            (FeedbackKind::Impression, "impression", "感想"),
            (FeedbackKind::Request, "request", "要望"),
            (FeedbackKind::Bug, "bug", "不具合"),
        ] {
            assert_eq!(kind.as_schema_str(), schema);
            assert_eq!(FeedbackKind::from_schema_str(schema), Some(kind));
            assert_eq!(kind.display_name(), display);
        }
        assert_eq!(FeedbackKind::from_schema_str("other"), None);
    }

    #[test]
    fn validates_body_boundaries_by_character_count() {
        assert_eq!(validate_body(""), Err(FeedbackBodyError::Empty));
        assert_eq!(validate_body(" \n\t"), Err(FeedbackBodyError::Empty));
        assert_eq!(validate_body(&"あ".repeat(4000)), Ok(()));
        assert_eq!(
            validate_body(&"😀".repeat(4001)),
            Err(FeedbackBodyError::TooLong)
        );
    }

    #[test]
    fn json_escapes_untrusted_body_and_preserves_unicode() {
        let payload = FeedbackPayload {
            kind: FeedbackKind::Bug,
            body: "URL https://example.test/a?x=1 \"quote\" \\ slash\n\u{0000}\u{001f} 日本語 😀"
                .to_owned(),
            app_version: "0.1.1".to_owned(),
            os: "windows".to_owned(),
            arch: "x86_64".to_owned(),
        };

        assert_eq!(
            payload.to_json(),
            "{\"schema_version\":1,\"kind\":\"bug\",\"body\":\"URL https://example.test/a?x=1 \\\"quote\\\" \\\\ slash\\n\\u0000\\u001F 日本語 😀\",\"app_version\":\"0.1.1\",\"os\":\"windows\",\"arch\":\"x86_64\"}"
        );
    }

    #[test]
    fn payload_has_only_the_allowed_metadata_fields() {
        let payload = FeedbackPayload {
            kind: FeedbackKind::Impression,
            body: "使いやすい".to_owned(),
            app_version: "0.1.1".to_owned(),
            os: "windows".to_owned(),
            arch: "x86_64".to_owned(),
        };
        let json = payload.to_json();
        assert_eq!(json.matches("\":").count(), 6);
        for forbidden in ["path", "file_name", "root", "hostname", "username"] {
            assert!(!json.contains(forbidden));
        }
    }
}
