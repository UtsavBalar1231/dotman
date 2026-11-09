//! `dotman key init` with an identity already in place.

mod common;

use common::{TestEnv, age_installed};
use std::fs;

#[test]
fn key_init_lists_the_recipient_of_an_identity_a_killed_run_left_unlisted() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    let identity = env.xdg_config().join("dotman/age-identity.txt");
    let created = fs::read(&identity).expect("identity");
    let mut config = env.repo_config();
    let recipients = std::mem::take(&mut config.age.recipients);
    assert_eq!(recipients.len(), 1);
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");

    let err = env.ok(&["key", "init"]);
    assert!(
        err.contains("added its recipient") && err.contains("password manager"),
        "{err}"
    );
    assert_eq!(env.repo_config().age.recipients, recipients);
    assert_eq!(fs::read(&identity).expect("identity"), created);

    let err = env.fails(&["key", "init"]);
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(env.repo_config().age.recipients, recipients);
}
