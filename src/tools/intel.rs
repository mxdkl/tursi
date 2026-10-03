//! code_intel: batched queries against the LSP manager (§4.1).

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::lsp::Query;
use crate::tools::Toolbox;

#[derive(Deserialize)]
pub struct IntelArgs {
    pub queries: Vec<Query>,
}

/// Run each query, concatenate distillates in call order. A failing query
/// reports inline and never kills its batchmates.
pub async fn run(tb: &mut Toolbox, args: &Value) -> Result<String> {
    let args: IntelArgs = serde_json::from_value(args.clone())?;
    if args.queries.is_empty() {
        anyhow::bail!("no queries given");
    }
    let mut out = String::new();
    for q in &args.queries {
        out.push_str(&format!("── {} ──\n", describe(q)));
        match tb.lsp.query(q).await {
            Ok(result) => out.push_str(&result),
            Err(e) => out.push_str(&format!("error: {e:#}")),
        }
        out.push('\n');
    }
    Ok(out.trim_end().to_string())
}

fn describe(q: &Query) -> String {
    let target = match (&q.symbol, &q.file) {
        (Some(symbol), _) => symbol.clone(),
        (None, Some(file)) => match q.line {
            Some(line) => format!("{}:{line}", file.display()),
            None => file.display().to_string(),
        },
        (None, None) => "?".to_string(),
    };
    format!("{:?} {target}", q.action).to_lowercase()
}
