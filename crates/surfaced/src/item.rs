//! The display-item model — what a `send` pushes to a surface (DESIGN §10.1).
//!
//! A [`DisplayItem`] is the payload the MCP `send` verb and the control-socket
//! `send-item` carry. It is internally tagged by `type` so the wire form is the
//! `{navigate|pdf|text|link|html|script, ...}` shape the design fixes. Each push
//! becomes an [`InboxItem`]: a display item plus its identity, timestamp, and
//! whether it also promotes to the surface's main view.

use serde::{Deserialize, Serialize};

/// A single thing pushed to a surface for display (DESIGN §10.1 built-in types).
///
/// `navigate` renders a third-party URL inside a sandboxed iframe (untrusted web
/// content is contained, never injected). `pdf` renders in a plain iframe so the
/// browser's built-in PDF viewer works — a sandboxed iframe blocks it — which is
/// safe because the PDF is cross-origin/passive and the sender is trusted.
/// `html`/`script` are same-origin to `surfaced` and fully trusted — safe because
/// `send` is trust-boundary-gated (DESIGN §8): only cluster-authenticated hosts
/// may push.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DisplayItem {
    /// An iframe pointed at a third-party URL (one thing among many).
    Navigate { url: String },
    /// A PDF viewer (the "put this manual page on my phone" case). An optional
    /// `page` deep-links into the document (1-based), for the many-hundred-page
    /// manuals where opening at page 1 is useless.
    Pdf {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page: Option<u32>,
    },
    /// A plain-text note.
    Text { body: String },
    /// A clickable link, optionally titled.
    Link {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Arbitrary same-origin DOM we control.
    Html { markup: String },
    /// Arbitrary JS run in the surface page — the full scripting escape hatch.
    Script { code: String },
}

impl DisplayItem {
    /// A short, human-readable label for the inbox feed (never renders the raw
    /// body/markup/code — just names the item).
    pub fn summary(&self) -> String {
        match self {
            DisplayItem::Navigate { url } => format!("navigate → {url}"),
            DisplayItem::Pdf { url, page } => match page {
                Some(p) => format!("pdf → {url} (p.{p})"),
                None => format!("pdf → {url}"),
            },
            DisplayItem::Text { body } => {
                let head: String = body.chars().take(60).collect();
                format!("text: {head}")
            }
            DisplayItem::Link { url, title } => match title {
                Some(t) => format!("link: {t} ({url})"),
                None => format!("link: {url}"),
            },
            DisplayItem::Html { .. } => "html".to_string(),
            DisplayItem::Script { .. } => "script".to_string(),
        }
    }
}

/// One entry in a surface's durable, ordered inbox: a display item with its
/// identity, push time (unix millis), and whether it promoted to the main view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    /// Stable id assigned at push (a v4 UUID); the main-view selector key.
    pub id: String,
    /// Push time, unix epoch milliseconds.
    pub ts: u64,
    /// Whether this push also promoted to the main view (§10.1).
    pub promote: bool,
    /// The pushed display item.
    pub item: DisplayItem,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_page_is_optional_and_round_trips() {
        // No page: the field is omitted from the wire form entirely.
        let plain: DisplayItem = serde_json::from_str(r#"{"type":"pdf","url":"u"}"#).unwrap();
        assert_eq!(
            plain,
            DisplayItem::Pdf {
                url: "u".into(),
                page: None
            }
        );
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"type":"pdf","url":"u"}"#
        );

        // With a page: carried through and shown in the feed summary.
        let paged: DisplayItem =
            serde_json::from_str(r#"{"type":"pdf","url":"u","page":42}"#).unwrap();
        assert_eq!(
            paged,
            DisplayItem::Pdf {
                url: "u".into(),
                page: Some(42)
            }
        );
        assert!(paged.summary().contains("p.42"));
    }
}
