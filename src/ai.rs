//! Adaptive SMS parsing — the Phase 2 differentiator.
//!
//! When the regex parsers miss (MFS operators drift their SMS templates without
//! notice), we fall back to an LLM extraction pass across a chain of
//! OpenAI-compatible providers (OpenRouter, Opencode Zen — multiple free tiers).
//! First provider to return a valid, complete extraction wins. Every attempt is
//! logged to `ai_parse_log` so drifted formats become visible and future regex
//! updates can be suggested from real data.
//!
//! No API keys configured → the fallback is silently skipped and the engine
//! behaves exactly like Phase 1 (regex-only). Author: Mohammad Sijan (SpritexAI).

use crate::db::Db;
use crate::gateway::ParsedTxn;
use serde::Deserialize;
use std::time::Duration;

pub struct Provider {
    pub name: &'static str,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Build the provider chain from the environment. Order = priority.
pub fn providers_from_env() -> Vec<Provider> {
    let mut chain = Vec::new();
    if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
        if !key.is_empty() {
            chain.push(Provider {
                name: "openrouter",
                base_url: env_or("OPENROUTER_BASE", "https://openrouter.ai/api/v1"),
                api_key: key,
                model: env_or("OPENROUTER_MODEL", "meta-llama/llama-3.3-70b-instruct:free"),
            });
        }
    }
    if let Ok(key) = std::env::var("OPENCODE_ZEN_API_KEY") {
        if !key.is_empty() {
            chain.push(Provider {
                name: "opencode-zen",
                base_url: env_or("OPENCODE_ZEN_BASE", "https://opencode.ai/zen/v1"),
                api_key: key,
                model: env_or("OPENCODE_ZEN_MODEL", "grok-code"),
            });
        }
    }
    chain
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

const EXTRACTION_PROMPT: &str = "You extract fields from a mobile-money (bKash/Nagad \
Bangladesh) confirmation SMS. Reply with ONLY a JSON object, no prose, no code fences: \
{\"txn_id\": string, \"amount_bdt\": number, \"sender_msisdn\": string|null}. \
txn_id is the transaction reference (TrxID/TxnID). amount_bdt is the received amount \
in taka. sender_msisdn is the 11-digit sender number starting 01, or null. \
If the SMS is not a money-received confirmation, reply exactly: {\"txn_id\": null}";

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}
#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

#[derive(Deserialize)]
struct Extraction {
    txn_id: Option<String>,
    amount_bdt: Option<f64>,
    sender_msisdn: Option<String>,
}

/// Parse the model's reply into a ParsedTxn. Tolerates code fences and stray prose
/// around the JSON object — models don't always follow "ONLY JSON" perfectly.
pub fn parse_extraction(gateway: &'static str, content: &str) -> Option<ParsedTxn> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    let ex: Extraction = serde_json::from_str(&content[start..=end]).ok()?;

    let txn_id = ex.txn_id?.trim().to_string();
    if txn_id.is_empty() {
        return None;
    }
    let amount = ex.amount_bdt?;
    // Reject NaN/inf/zero/negative — this is money.
    if !amount.is_finite() || amount <= 0.0 {
        return None;
    }
    Some(ParsedTxn {
        gateway,
        txn_id,
        amount_minor: (amount * 100.0).round() as i64,
        sender_msisdn: ex.sender_msisdn.filter(|s| !s.is_empty()),
    })
}

/// Try each provider in order; first valid extraction wins. Logs every attempt.
pub async fn extract(
    db: &Db,
    gateway: &'static str,
    raw_body: &str,
    raw_sha256: &str,
) -> Option<ParsedTxn> {
    let chain = providers_from_env();
    if chain.is_empty() {
        return None;
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?;

    for p in &chain {
        let result = call_provider(&client, p, raw_body).await;
        let parsed = result.as_deref().and_then(|c| parse_extraction(gateway, c));

        let _ = sqlx::query(
            "INSERT INTO ai_parse_log (gateway, raw_sha256, provider, success, txn_id, amount_minor, sender_msisdn) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(gateway)
        .bind(raw_sha256)
        .bind(p.name)
        .bind(parsed.is_some() as i64)
        .bind(parsed.as_ref().map(|t| t.txn_id.clone()))
        .bind(parsed.as_ref().map(|t| t.amount_minor))
        .bind(parsed.as_ref().and_then(|t| t.sender_msisdn.clone()))
        .execute(db)
        .await;

        match parsed {
            Some(txn) => {
                tracing::info!(provider = p.name, txn_id = %txn.txn_id, "AI fallback recovered SMS parse");
                return Some(txn);
            }
            None => {
                tracing::warn!(
                    provider = p.name,
                    "AI provider failed to extract, trying next"
                );
            }
        }
    }
    None
}

async fn call_provider(client: &reqwest::Client, p: &Provider, sms: &str) -> Option<String> {
    let body = serde_json::json!({
        "model": p.model,
        "messages": [
            { "role": "system", "content": EXTRACTION_PROMPT },
            { "role": "user", "content": sms },
        ],
        "temperature": 0,
    });

    let resp = client
        .post(format!("{}/chat/completions", p.base_url))
        .bearer_auth(&p.api_key)
        .json(&body)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!(provider = p.name, status = %resp.status(), "AI provider returned error");
        return None;
    }
    let chat: ChatResponse = resp.json().await.ok()?;
    chat.choices.into_iter().next().map(|c| c.message.content)
}

#[cfg(test)]
mod tests {
    use super::parse_extraction;

    #[test]
    fn parses_clean_json() {
        let t = parse_extraction(
            "bkash",
            r#"{"txn_id": "ABC123", "amount_bdt": 500.0, "sender_msisdn": "01710000000"}"#,
        )
        .unwrap();
        assert_eq!(t.txn_id, "ABC123");
        assert_eq!(t.amount_minor, 50_000);
        assert_eq!(t.sender_msisdn.as_deref(), Some("01710000000"));
    }

    #[test]
    fn tolerates_code_fences_and_prose() {
        let content = "Sure! Here is the JSON:\n```json\n{\"txn_id\": \"XYZ9\", \"amount_bdt\": 120.5, \"sender_msisdn\": null}\n```";
        let t = parse_extraction("nagad", content).unwrap();
        assert_eq!(t.txn_id, "XYZ9");
        assert_eq!(t.amount_minor, 12_050);
        assert!(t.sender_msisdn.is_none());
    }

    #[test]
    fn rejects_non_payment_and_garbage() {
        assert!(parse_extraction("bkash", r#"{"txn_id": null}"#).is_none());
        assert!(parse_extraction("bkash", "no json at all").is_none());
        assert!(parse_extraction("bkash", r#"{"txn_id": "A", "amount_bdt": -5}"#).is_none());
        assert!(parse_extraction("bkash", r#"{"txn_id": "", "amount_bdt": 10}"#).is_none());
    }
}
