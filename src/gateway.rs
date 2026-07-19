//! Payment gateways.
//!
//! Each MFS integration implements [`Gateway`]. For the SMS-based methods (bKash,
//! Nagad) the core work is parsing the confirmation SMS into a normalized
//! [`ParsedTxn`]. New gateways are added by implementing the trait — the engine
//! core never changes.

use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTxn {
    pub gateway: &'static str,
    pub txn_id: String,
    pub amount_minor: i64,
    pub sender_msisdn: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    #[error("SMS did not match the {0} format")]
    NoMatch(&'static str),
    #[error("could not parse amount from SMS")]
    BadAmount,
}

pub trait Gateway {
    fn id(&self) -> &'static str;
    /// Best-effort structured extraction from a raw confirmation SMS.
    fn parse_sms(&self, body: &str) -> Result<ParsedTxn, ParseError>;
}

/// Resolve a gateway by its sender/shortcode identifier.
pub fn resolve(gateway: &str) -> Option<Box<dyn Gateway>> {
    match gateway.to_ascii_lowercase().as_str() {
        "bkash" => Some(Box::new(Bkash)),
        "nagad" => Some(Box::new(Nagad)),
        _ => None,
    }
}

/// "1,234.56" | "500" | "1500.00" -> minor units (poisha). BDT has 2 decimals.
fn amount_to_minor(raw: &str) -> Result<i64, ParseError> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let value: f64 = cleaned.parse().map_err(|_| ParseError::BadAmount)?;
    // Round to nearest poisha to absorb float representation error.
    Ok((value * 100.0).round() as i64)
}

// bKash confirmation SMS. Formats drift when bKash updates their templates —
// ponytail: regex is the v1 path; the Phase-2 LLM fallback recovers drift and
// auto-suggests updates to these patterns. Tune against captured live samples.
static BKASH_TXID: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)TrxID[:\s]+([A-Z0-9]+)").unwrap());
static BKASH_AMOUNT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:received|Tk\.?)\s*Tk?\.?\s*([\d,]+(?:\.\d+)?)").unwrap());
static BKASH_SENDER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)from\s+(01\d{9})").unwrap());

pub struct Bkash;

impl Gateway for Bkash {
    fn id(&self) -> &'static str {
        "bkash"
    }

    fn parse_sms(&self, body: &str) -> Result<ParsedTxn, ParseError> {
        let txn_id = BKASH_TXID
            .captures(body)
            .map(|c| c[1].to_string())
            .ok_or(ParseError::NoMatch("bkash"))?;
        let amount_minor = BKASH_AMOUNT
            .captures(body)
            .ok_or(ParseError::NoMatch("bkash"))
            .and_then(|c| amount_to_minor(&c[1]))?;
        let sender_msisdn = BKASH_SENDER.captures(body).map(|c| c[1].to_string());

        Ok(ParsedTxn {
            gateway: "bkash",
            txn_id,
            amount_minor,
            sender_msisdn,
        })
    }
}

// Nagad confirmation SMS. Same drift caveat as bKash.
static NAGAD_TXID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:TxnID|TrxID)[:\s]+([A-Z0-9]+)").unwrap());
static NAGAD_AMOUNT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)Amount[:\s]+Tk\.?\s*([\d,]+(?:\.\d+)?)").unwrap());
static NAGAD_SENDER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)Sender[:\s]+(01\d{9})").unwrap());

pub struct Nagad;

impl Gateway for Nagad {
    fn id(&self) -> &'static str {
        "nagad"
    }

    fn parse_sms(&self, body: &str) -> Result<ParsedTxn, ParseError> {
        let txn_id = NAGAD_TXID
            .captures(body)
            .map(|c| c[1].to_string())
            .ok_or(ParseError::NoMatch("nagad"))?;
        let amount_minor = NAGAD_AMOUNT
            .captures(body)
            .ok_or(ParseError::NoMatch("nagad"))
            .and_then(|c| amount_to_minor(&c[1]))?;
        let sender_msisdn = NAGAD_SENDER.captures(body).map(|c| c[1].to_string());

        Ok(ParsedTxn {
            gateway: "nagad",
            txn_id,
            amount_minor,
            sender_msisdn,
        })
    }
}
