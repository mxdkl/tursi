//! ask_user (§4.4): a TUI overlay, blocking in BOTH approval modes — AUTO
//! means "don't ask permission", not "never talk to me".

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::bus::{AskRequest, EventKind, UiHandle};
use crate::tools::Toolbox;

#[derive(Deserialize)]
pub struct AskArgs {
    pub question: String,
    pub options: Option<Vec<String>>,
}

/// Attended: overlay + terminal bell (rendered UI-side), block for the
/// answer. AFK: return immediately with the §4.4 contract text; the
/// question/assumption pair surfaces in the transcript for review.
pub async fn run(tb: &Toolbox, args: &Value, ui: &UiHandle) -> Result<String> {
    let args: AskArgs = serde_json::from_value(args.clone())?;
    if tb.afk {
        return Ok(format!(
            "user unavailable (AFK) — proceed on best judgment and state the assumption \
             in your summary. Your question was: {}",
            args.question
        ));
    }
    let (reply, rx) = oneshot::channel();
    ui.send(EventKind::Ask(AskRequest {
        question: args.question,
        options: args.options.unwrap_or_default(),
        reply,
    }))
    .await;
    let answer = rx.await?;
    Ok(format!("user answered: {answer}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil;

    #[tokio::test]
    async fn afk_mode_returns_best_judgment_contract_immediately() {
        let dir = testutil::tmp("ask-afk");
        let (mut tb, ui, _rx) = testutil::toolbox(&dir);
        tb.afk = true;
        let out = run(&tb, &serde_json::json!({"question": "tabs or spaces?"}), &ui)
            .await
            .unwrap();
        assert!(out.contains("best judgment"));
        assert!(out.contains("tabs or spaces?"));
    }

    #[tokio::test]
    async fn attended_mode_blocks_until_the_overlay_answers() {
        let dir = testutil::tmp("ask-wait");
        let (tb, ui, mut rx) = testutil::toolbox(&dir);
        let answerer = tokio::spawn(async move {
            if let Some(event) = rx.recv().await {
                if let crate::bus::EventKind::Ask(req) = event.kind {
                    let _ = req.reply.send("spaces".to_string());
                }
            }
        });
        let out = run(&tb, &serde_json::json!({"question": "tabs or spaces?"}), &ui)
            .await
            .unwrap();
        answerer.await.unwrap();
        assert_eq!(out, "user answered: spaces");
    }
}
