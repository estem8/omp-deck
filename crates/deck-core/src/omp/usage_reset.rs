//! `omp usage reset`: the saved rate-limit resets (Codex / Claude) of the stored accounts, and
//! spending one. Verified against omp 18.16.0 with an account-less sandbox (the list shape is
//! pinned from omp's source, `cli/usage-cli.ts` + `ai/src/auth/resets.ts`; spending needs a real
//! account and is not exercised by the tests).

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::plugins::json_tail;
use super::{Omp, OmpError, failed};

/// omp prints credential and credit ids as numbers or strings; both read as text.
fn id_text<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!("not an id: {other}"))),
    }
}

fn opt_id_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// One saved reset ("credit") of an account.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCredit {
    #[serde(default, deserialize_with = "opt_id_text")]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// Provider reset program/family.
    #[serde(default)]
    pub program: Option<String>,
    /// Backend status, e.g. `available`, `redeemed`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub remaining_count: Option<u32>,
    /// The provider says it can be redeemed now.
    #[serde(default)]
    pub usable: Option<bool>,
    /// Redeeming needs an exhausted covered limit.
    #[serde(default)]
    pub requires_limit: Option<bool>,
    /// Limit ids this credit resets.
    #[serde(default)]
    pub clears: Vec<String>,
    /// Limit ids currently preventing redemption.
    #[serde(default)]
    pub blocking: Vec<String>,
    #[serde(default)]
    pub used_fractions: BTreeMap<String, f64>,
    /// ISO timestamps.
    #[serde(default)]
    pub granted_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// One stored account's saved-reset status.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetAccount {
    /// `openai-codex` or `anthropic`.
    pub provider: String,
    /// omp's credential id (what [`Omp::usage_reset_spend`] names).
    #[serde(deserialize_with = "id_text")]
    pub credential_id: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub org_id: Option<String>,
    #[serde(default)]
    pub org_name: Option<String>,
    /// Banked resets, usable or not.
    #[serde(default)]
    pub available_count: u32,
    /// Resets that can be redeemed now.
    #[serde(default)]
    pub redeemable_count: u32,
    #[serde(default, deserialize_with = "opt_id_text")]
    pub next_credit_id: Option<String>,
    /// ISO timestamp of the soonest expiring credit.
    #[serde(default)]
    pub soonest_expiry: Option<String>,
    /// Why nothing is redeemable (provider text).
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// The account could not be reached (token refresh or list failed).
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub credits: Vec<ResetCredit>,
}

/// `omp usage reset [--provider p] --json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetAccounts {
    /// Unix milliseconds.
    #[serde(default)]
    pub generated_at: u64,
    #[serde(default)]
    pub accounts: Vec<ResetAccount>,
}

/// `provider` and `credential` become the `<provider>/<credential id>` target.
fn target(provider: &str, credential: &str) -> Result<String, OmpError> {
    let bad = |s: &str| s.is_empty() || s.starts_with('-') || s.contains(['/', '\0', '\n']);
    if bad(provider) || bad(credential) {
        return Err(failed(format!(
            "Invalid reset target: {provider}/{credential}"
        )));
    }
    Ok(format!("{provider}/{credential}"))
}

impl Omp {
    /// The saved resets of every stored Codex and Claude account, or of one `provider`
    /// (`openai-codex` / `anthropic`). No stored account → an empty list, exit 0. Queries the
    /// providers: can take seconds.
    pub fn usage_resets(&self, provider: Option<&str>) -> Result<ResetAccounts, OmpError> {
        let mut args = vec!["usage", "reset", "--json"];
        if let Some(p) = provider {
            if p.is_empty() || p.starts_with('-') {
                return Err(failed(format!("Invalid provider: {p}")));
            }
            args.extend(["--provider", p]);
        }
        let out = self.run(&args, None)?;
        serde_json::from_str(json_tail(&out.stdout).trim())
            .map_err(|e| OmpError::Parse(format!("{e}: {}", out.stdout.trim())))
    }

    /// Spends one saved reset of the stored account `credential` of `provider`
    /// (`omp usage reset <provider>/<credential>`). omp takes its cross-process fence, rechecks
    /// the Codex balance and bounds the redeem at 15 s; a refusal (nothing to spend, another omp
    /// spent first, network) exits non-zero and is the error message. Returns omp's one-line
    /// confirmation. Not idempotent: never retry blindly.
    pub fn usage_reset_spend(&self, provider: &str, credential: &str) -> Result<String, OmpError> {
        let target = target(provider, credential)?;
        let out = self.run(&["usage", "reset", &target], None)?;
        Ok(out.stdout.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accounts_read_numeric_ids_and_typed_credits() {
        let list: ResetAccounts = serde_json::from_value(json!({
            "generatedAt": 1793688531127u64,
            "accounts": [{
                "provider": "openai-codex", "credentialId": 4, "email": "a@b.c",
                "availableCount": 2, "redeemableCount": 1, "nextCreditId": "cr_1",
                "soonestExpiry": "2026-12-01T00:00:00.000Z", "futureField": true,
                "credits": [{
                    "id": "cr_1", "title": "Weekly", "program": "codex", "status": "available",
                    "remainingCount": 1, "usable": true, "requiresLimit": false,
                    "clears": ["weekly"], "usedFractions": { "weekly": 0.5 },
                    "grantedAt": "2026-11-01T00:00:00Z", "expiresAt": "2026-12-01T00:00:00Z"
                }, { "id": 7 }]
            }, { "provider": "anthropic", "credentialId": "x9", "error": "token refresh failed" }]
        }))
        .unwrap();
        let (a, b) = (&list.accounts[0], &list.accounts[1]);
        assert_eq!(a.credential_id, "4");
        assert_eq!((a.available_count, a.redeemable_count), (2, 1));
        assert_eq!(a.credits[0].used_fractions["weekly"], 0.5);
        assert_eq!(a.credits[1].id.as_deref(), Some("7"));
        assert_eq!(b.credential_id, "x9");
        assert_eq!(b.error.as_deref(), Some("token refresh failed"));
        assert_eq!(b.available_count, 0, "omitted counts read as zero");
        assert!(b.credits.is_empty());
    }

    #[test]
    fn empty_listing_decodes() {
        let list: ResetAccounts =
            serde_json::from_str("{\n  \"generatedAt\": 1793688531127,\n  \"accounts\": []\n}")
                .unwrap();
        assert!(list.accounts.is_empty());
        assert_eq!(list.generated_at, 1793688531127);
    }

    #[test]
    fn spend_target_rejects_ids_that_would_change_the_command() {
        assert_eq!(target("anthropic", "12").unwrap(), "anthropic/12");
        assert!(target("anthropic", "1/2").is_err());
        assert!(target("-x", "1").is_err());
        assert!(target("a", "").is_err());
    }
}
