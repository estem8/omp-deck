//! The Usage view's "Saved resets" page (omp's `/usage reset`): what the stored accounts have
//! banked (`omp usage reset --json`) as view rows, and the state of reading and spending them.
//! Pure logic — [`crate::usage_view`] paints it and `app::usage` runs the gateway requests.
//!
//! Wording and eligibility follow omp's own selector: an account is spendable when it has at
//! least one saved reset; the empty-state texts are omp's.

use deck_core::ctl::LiveAccount;
use deck_core::omp::{ResetAccount, ResetAccounts};

/// omp's text when no account is stored at all.
pub const NO_ACCOUNTS: &str = "No provider accounts found. Use /login to add one.";
/// omp's text when nothing is spendable and some account could not be queried.
pub const UNREACHABLE: &str =
    "No saved resets available — some accounts couldn't be reached (try /login).";
/// omp's text when every account answered and none has a reset to spend.
pub const NOTHING_TO_SPEND: &str = "No saved rate-limit resets available to spend right now.";

/// Reading and spending state of the page.
#[derive(Default)]
pub struct ResetsState {
    /// The last list omp gave (kept while it is read again).
    pub accounts: Option<ResetAccounts>,
    /// The last read failed with this.
    pub error: Option<String>,
    /// A read is out.
    pub loading: bool,
    /// The list was asked for since the view opened (the page reads it once on first showing).
    pub requested: bool,
    /// A spend is out: (provider, credential).
    pub spending: Option<(String, String)>,
}

impl ResetsState {
    /// The view opened (or was asked to refresh): the next showing of the page reads the list.
    pub fn invalidate(&mut self) {
        self.requested = false;
    }

    /// A read was sent.
    pub fn begin_load(&mut self) {
        self.requested = true;
        self.loading = true;
        self.error = None;
    }

    /// The read finished.
    pub fn loaded(&mut self, result: Result<ResetAccounts, String>) {
        self.loading = false;
        match result {
            Ok(list) => {
                self.accounts = Some(list);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Whether this account's spend is the one in flight.
    pub fn is_spending(&self, provider: &str, credential: &str) -> bool {
        self.spending
            .as_ref()
            .is_some_and(|(p, c)| p == provider && c == credential)
    }
}

/// One stored account as the page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    /// omp's provider id (what the spend names).
    pub provider: String,
    /// omp's credential id (what the spend names).
    pub credential: String,
    /// "Codex" / "Claude".
    pub provider_label: String,
    /// "email · org".
    pub label: String,
    pub available: u32,
    pub redeemable: u32,
    /// Date of the soonest expiring saved reset.
    pub soonest: Option<String>,
    /// Why nothing can be redeemed now (provider text).
    pub reason: Option<String>,
    /// Why the account could not be queried.
    pub error: Option<String>,
    /// A saved reset can be spent (omp's selector lists exactly these).
    pub spendable: bool,
    /// The focused tab's omp session runs on this account.
    pub in_use: bool,
}

impl AccountRow {
    /// What the spend confirmation says.
    pub fn spend_question(&self) -> String {
        format!(
            "Spending 1 saved reset for {} ({}).",
            self.label, self.provider_label
        )
    }
}

fn provider_label(provider: &str) -> String {
    match provider {
        "openai-codex" => "Codex".to_owned(),
        "anthropic" => "Claude".to_owned(),
        other => other.to_owned(),
    }
}

fn non_blank(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// omp's account label: email, else account id, else "account"; plus the org name or id.
pub fn account_label(a: &ResetAccount) -> String {
    let who = non_blank(&a.email)
        .or_else(|| non_blank(&a.account_id))
        .unwrap_or("account");
    match non_blank(&a.org_name).or_else(|| non_blank(&a.org_id)) {
        Some(org) => format!("{who} · {org}"),
        None => who.to_owned(),
    }
}

/// `2026-12-01T00:00:00.000Z` → `2026-12-01`; anything else as is.
fn date_of(iso: &str) -> String {
    let b = iso.as_bytes();
    let dated = iso.len() >= 10
        && b[4] == b'-'
        && b[7] == b'-'
        && iso.is_char_boundary(10)
        && iso[..10]
            .chars()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if dated {
        iso[..10].to_owned()
    } else {
        iso.to_owned()
    }
}

/// Whether the focused tab's session uses `a`: the stored account of its provider that is the
/// account the session runs on.
fn in_use(a: &ResetAccount, session: Option<&LiveAccount>) -> bool {
    session.is_some_and(|s| {
        s.provider == a.provider
            && s.credentials
                .iter()
                .any(|c| c.id == a.credential_id && c.identity.same_account(&s.identity))
    })
}

/// The page's rows, one per stored account, in omp's order.
pub fn rows(list: &ResetAccounts, session: Option<&LiveAccount>) -> Vec<AccountRow> {
    list.accounts
        .iter()
        .map(|a| AccountRow {
            provider: a.provider.clone(),
            credential: a.credential_id.clone(),
            provider_label: provider_label(&a.provider),
            label: account_label(a),
            available: a.available_count,
            redeemable: a.redeemable_count,
            soonest: non_blank(&a.soonest_expiry).map(date_of),
            reason: non_blank(&a.unavailable_reason).map(str::to_owned),
            error: non_blank(&a.error).map(str::to_owned),
            spendable: a.available_count > 0,
            in_use: in_use(a, session),
        })
        .collect()
}

/// omp's message when no account can spend a reset; `None` when at least one can.
pub fn empty_message(list: &ResetAccounts) -> Option<&'static str> {
    if list.accounts.is_empty() {
        Some(NO_ACCOUNTS)
    } else if list.accounts.iter().any(|a| a.available_count > 0) {
        None
    } else if list.accounts.iter().any(|a| non_blank(&a.error).is_some()) {
        Some(UNREACHABLE)
    } else {
        Some(NOTHING_TO_SPEND)
    }
}

#[cfg(test)]
mod tests {
    use deck_core::ctl::LiveCredential;
    use deck_core::usage::OAuthIdentity;

    use super::*;

    fn account(provider: &str, id: &str, email: &str, available: u32) -> ResetAccount {
        ResetAccount {
            provider: provider.to_owned(),
            credential_id: id.to_owned(),
            email: Some(email.to_owned()),
            available_count: available,
            redeemable_count: available.min(1),
            ..ResetAccount::default()
        }
    }

    fn identity(email: &str) -> OAuthIdentity {
        OAuthIdentity {
            email: Some(email.to_owned()),
            ..OAuthIdentity::default()
        }
    }

    fn live(provider: &str, running: &str, stored: &[(&str, &str)]) -> LiveAccount {
        LiveAccount {
            provider: provider.to_owned(),
            identity: identity(running),
            pinned: false,
            credentials: stored
                .iter()
                .map(|(id, email)| LiveCredential {
                    id: (*id).to_owned(),
                    identity: identity(email),
                })
                .collect(),
        }
    }

    #[test]
    fn a_row_carries_what_the_page_shows() {
        let mut a = account("openai-codex", "4", "me@x.io", 2);
        a.org_name = Some("Acme".to_owned());
        a.soonest_expiry = Some("2026-12-01T00:00:00.000Z".to_owned());
        a.unavailable_reason = Some("  ".to_owned());
        let list = ResetAccounts {
            generated_at: 1,
            accounts: vec![a],
        };
        let r = &rows(&list, None)[0];
        assert_eq!(r.provider_label, "Codex");
        assert_eq!(r.label, "me@x.io · Acme");
        assert_eq!((r.available, r.redeemable), (2, 1));
        assert_eq!(r.soonest.as_deref(), Some("2026-12-01"));
        assert!(r.spendable && !r.in_use);
        assert_eq!(r.reason, None, "a blank reason is no reason");
        assert_eq!(
            r.spend_question(),
            "Spending 1 saved reset for me@x.io · Acme (Codex)."
        );
    }

    #[test]
    fn the_label_falls_back_like_omps_selector() {
        let mut a = ResetAccount {
            provider: "anthropic".to_owned(),
            ..ResetAccount::default()
        };
        assert_eq!(account_label(&a), "account");
        a.account_id = Some("acct_1".to_owned());
        a.org_id = Some("org_9".to_owned());
        assert_eq!(account_label(&a), "acct_1 · org_9");
        a.email = Some("a@b.c".to_owned());
        a.org_name = Some("Team".to_owned());
        assert_eq!(account_label(&a), "a@b.c · Team");
        assert_eq!(provider_label("anthropic"), "Claude");
        assert_eq!(provider_label("something"), "something");
    }

    #[test]
    fn an_account_without_a_reset_is_not_spendable_and_says_why() {
        let mut a = account("anthropic", "x", "a@b.c", 0);
        a.unavailable_reason = Some("No resets banked".to_owned());
        a.error = Some("token refresh failed".to_owned());
        let list = ResetAccounts {
            generated_at: 1,
            accounts: vec![a],
        };
        let r = &rows(&list, None)[0];
        assert!(!r.spendable);
        assert_eq!(r.reason.as_deref(), Some("No resets banked"));
        assert_eq!(r.error.as_deref(), Some("token refresh failed"));
    }

    #[test]
    fn the_empty_messages_are_omps() {
        let none = ResetAccounts::default();
        assert_eq!(empty_message(&none), Some(NO_ACCOUNTS));

        let quiet = ResetAccounts {
            generated_at: 1,
            accounts: vec![account("anthropic", "x", "a@b.c", 0)],
        };
        assert_eq!(empty_message(&quiet), Some(NOTHING_TO_SPEND));

        let mut broken = account("openai-codex", "y", "d@e.f", 0);
        broken.error = Some("offline".to_owned());
        let unreachable = ResetAccounts {
            generated_at: 1,
            accounts: vec![account("anthropic", "x", "a@b.c", 0), broken],
        };
        assert_eq!(empty_message(&unreachable), Some(UNREACHABLE));

        let spendable = ResetAccounts {
            generated_at: 1,
            accounts: vec![account("anthropic", "x", "a@b.c", 1)],
        };
        assert_eq!(empty_message(&spendable), None);
    }

    #[test]
    fn the_account_the_focused_session_runs_on_is_marked() {
        let list = ResetAccounts {
            generated_at: 1,
            accounts: vec![
                account("anthropic", "c1", "one@x.io", 1),
                account("anthropic", "c2", "two@x.io", 1),
                account("openai-codex", "c2", "two@x.io", 1),
            ],
        };
        let session = live(
            "anthropic",
            "two@x.io",
            &[("c1", "one@x.io"), ("c2", "two@x.io")],
        );
        let marks: Vec<bool> = rows(&list, Some(&session))
            .iter()
            .map(|r| r.in_use)
            .collect();
        assert_eq!(
            marks,
            [false, true, false],
            "the same credential id of another provider is not it"
        );
        assert!(rows(&list, None).iter().all(|r| !r.in_use));
    }

    #[test]
    fn the_state_tracks_reads_and_spends() {
        let mut s = ResetsState::default();
        assert!(!s.requested);
        s.begin_load();
        assert!(s.loading && s.requested);
        s.loaded(Err("omp failed".to_owned()));
        assert!(!s.loading);
        assert_eq!(s.error.as_deref(), Some("omp failed"));
        s.loaded(Ok(ResetAccounts::default()));
        assert!(s.error.is_none() && s.accounts.is_some());
        s.begin_load();
        s.loaded(Err("again".to_owned()));
        assert!(
            s.accounts.is_some(),
            "the last list stays while a read fails"
        );
        s.invalidate();
        assert!(!s.requested);
        s.spending = Some(("anthropic".to_owned(), "c1".to_owned()));
        assert!(s.is_spending("anthropic", "c1") && !s.is_spending("anthropic", "c2"));
    }

    #[test]
    fn dates_are_cut_from_iso_stamps_only() {
        assert_eq!(date_of("2026-12-01T00:00:00Z"), "2026-12-01");
        assert_eq!(date_of("soon"), "soon");
        assert_eq!(date_of("2026/12/01T00"), "2026/12/01T00");
    }
}
