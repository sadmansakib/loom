//! Atomic account activation.
//!
//! ## Contract
//!
//! A [`Store`] holds a provider's accounts; at most one is active at a
//! time. [`set_active`] switches which account is active:
//!
//! * if `id` names no account in the store, it returns [`NoAccount`] and
//!   leaves every account exactly as it was — the switch is all-or-nothing,
//!   never a partial demotion;
//! * otherwise, the named account becomes active and every other account
//!   becomes inactive (including one that was already inactive), and the
//!   call returns `Ok`.
//!
//! Activating the account that is already active is a no-op that still
//! returns `Ok`.

/// One account in a [`Store`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub active: bool,
}

/// A provider's accounts.
#[derive(Debug, Clone, Default)]
pub struct Store {
    pub accounts: Vec<Account>,
}

/// `id` names no account in the store.
#[derive(Debug, PartialEq, Eq)]
pub struct NoAccount;

/// Switches which account is active, per the module contract.
pub fn set_active(store: &mut Store, id: &str) -> Result<(), NoAccount> {
    for account in &mut store.accounts {
        account.active = false;
    }
    match store.accounts.iter_mut().find(|account| account.id == id) {
        Some(account) => {
            account.active = true;
            Ok(())
        }
        None => Err(NoAccount),
    }
}
