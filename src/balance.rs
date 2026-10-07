//! Prepaid-credit balance of the inference provider, shown instead of the
//! harness's own token arithmetic: the number the provider will actually
//! charge. Cloudflare AI Gateway's credit endpoint is derived from the
//! provider's base URL; any other provider needs `[balance] url`.

use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{Config, Secrets};

pub struct Balance {
    http: reqwest::Client,
    url: String,
    token: String,
    headers: Vec<(String, String)>,
    /// JSON pointer to the number in the reply.
    pointer: String,
    /// The reply's unit per dollar (Cloudflare reports cents).
    divisor: f64,
}

impl Balance {
    /// None when the main model's provider has no known balance endpoint.
    pub fn from_config(config: &Config, secrets: &Secrets) -> Option<Arc<Balance>> {
        let (token, base) = secrets.provider_for(&config.model)?;
        let url = match &config.balance.url {
            Some(url) => url.clone(),
            None => cloudflare_credit_url(&base)?,
        };
        Some(Arc::new(Balance {
            http: reqwest::Client::builder().timeout(Duration::from_secs(10)).build().ok()?,
            url,
            token,
            headers: secrets.headers_for(&config.model),
            pointer: config.balance.pointer.clone(),
            divisor: config.balance.divisor,
        }))
    }

    /// Dollars remaining.
    pub async fn fetch(&self) -> Result<f64> {
        let mut req = self.http.get(&self.url).bearer_auth(&self.token).header("User-Agent", "tursi/0.1");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let body: Value = req.send().await.context("balance request")?.error_for_status().context("balance request")?.json().await.context("balance reply")?;
        let raw = body.pointer(&self.pointer).and_then(Value::as_f64).ok_or_else(|| anyhow::anyhow!("no number at {} in the balance reply", self.pointer))?;
        Ok(raw / self.divisor)
    }
}

/// `https://api.cloudflare.com/client/v4/accounts/<id>/ai/v1` →
/// `…/accounts/<id>/ai-gateway/billing/credit-balance`.
fn cloudflare_credit_url(base: &str) -> Option<String> {
    let (prefix, rest) = base.split_once("/accounts/")?;
    if !prefix.contains("api.cloudflare.com") {
        return None;
    }
    let account = rest.split('/').next().filter(|a| !a.is_empty())?;
    Some(format!("{prefix}/accounts/{account}/ai-gateway/billing/credit-balance"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_credit_url_is_derived_from_the_cloudflare_base_url() {
        assert_eq!(
            cloudflare_credit_url("https://api.cloudflare.com/client/v4/accounts/abc123/ai/v1").as_deref(),
            Some("https://api.cloudflare.com/client/v4/accounts/abc123/ai-gateway/billing/credit-balance")
        );
        assert!(cloudflare_credit_url("https://api.deepseek.com/v1").is_none());
    }
}
