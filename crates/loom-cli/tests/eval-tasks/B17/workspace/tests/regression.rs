use loom_eval_b17::{set_active, Account, NoAccount, Store};

fn store(accounts: &[(&str, bool)]) -> Store {
    Store {
        accounts: accounts
            .iter()
            .map(|(id, active)| Account {
                id: (*id).to_owned(),
                active: *active,
            })
            .collect(),
    }
}

fn active_id(store: &Store) -> Option<&str> {
    store
        .accounts
        .iter()
        .find(|account| account.active)
        .map(|account| account.id.as_str())
}

#[test]
fn an_absent_id_leaves_the_active_account_untouched() {
    let mut store = store(&[("work", true), ("home", false)]);
    let error = set_active(&mut store, "missing").unwrap_err();
    assert_eq!(error, NoAccount);
    assert_eq!(active_id(&store), Some("work"));
}

#[test]
fn an_absent_id_on_an_already_inactive_store_leaves_everyone_inactive() {
    // No account is active to begin with; a failed switch must not leave a
    // different account spuriously activated either.
    let mut store = store(&[("work", false), ("home", false)]);
    let error = set_active(&mut store, "missing").unwrap_err();
    assert_eq!(error, NoAccount);
    assert_eq!(active_id(&store), None);
}

#[test]
fn switching_to_a_real_account_demotes_the_previous_one() {
    let mut store = store(&[("work", true), ("home", false)]);
    set_active(&mut store, "home").unwrap();
    assert_eq!(active_id(&store), Some("home"));
}

#[test]
fn switching_to_the_already_active_account_is_a_no_op_ok() {
    let mut store = store(&[("work", true), ("home", false)]);
    set_active(&mut store, "work").unwrap();
    assert_eq!(active_id(&store), Some("work"));
}

#[test]
fn an_empty_store_rejects_any_id() {
    let mut store = store(&[]);
    let error = set_active(&mut store, "anything").unwrap_err();
    assert_eq!(error, NoAccount);
}

#[test]
fn switching_among_three_accounts_leaves_exactly_one_active() {
    let mut store = store(&[("a", true), ("b", false), ("c", false)]);
    set_active(&mut store, "c").unwrap();
    let active: Vec<&str> = store
        .accounts
        .iter()
        .filter(|account| account.active)
        .map(|account| account.id.as_str())
        .collect();
    assert_eq!(active, vec!["c"]);
}
