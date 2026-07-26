//! Phase 2 (@5) integration test: the write-path SQL intercept must return
//! `WriteRoute::Unsupported(reason)` for every shape enumerated as a v1
//! non-goal in the ADR (Amendment A2 Risk 8). This test runs against the
//! pure-parser `resolve_write_target` — no CLI / core wasm required — so it
//! is the one integration test that can run reliably before the sqlitewasm
//! read/write paths are e2e-plumbed.

use std::collections::HashMap;

use ducklink_host::at5_intercept::{resolve_write_target, WriteRoute};

fn attached_map(aliases: &[&str]) -> HashMap<String, String> {
    aliases.iter().map(|a| (a.to_string(), String::new())).collect()
}

fn assert_unsupported(sql: &str, reason_needle: &str) {
    let att = attached_map(&["mydb"]);
    let route = resolve_write_target(sql, &att)
        .unwrap_or_else(|| panic!("expected write-intercept to catch: {sql}"));
    match route {
        WriteRoute::Unsupported(reason) => assert!(
            reason.to_ascii_lowercase().contains(&reason_needle.to_ascii_lowercase()),
            "expected reason to mention '{reason_needle}', got: {reason}",
        ),
        other => panic!("expected Unsupported for {sql}, got {other:?}"),
    }
}

#[test]
fn returning_clause_is_rejected() {
    assert_unsupported(
        "INSERT INTO mydb.foo (id) VALUES (1) RETURNING id",
        "RETURNING",
    );
}

#[test]
fn on_conflict_do_nothing_is_rejected() {
    assert_unsupported(
        "INSERT INTO mydb.foo (id) VALUES (1) ON CONFLICT DO NOTHING",
        "ON CONFLICT",
    );
}

#[test]
fn on_duplicate_key_update_is_rejected() {
    assert_unsupported(
        "INSERT INTO mydb.foo (id) VALUES (1) ON DUPLICATE KEY UPDATE id = 2",
        "ON DUPLICATE",
    );
}

#[test]
fn cte_driven_insert_is_rejected() {
    assert_unsupported(
        "WITH x AS (SELECT 1) INSERT INTO mydb.foo SELECT * FROM x",
        "CTE",
    );
}

#[test]
fn insert_from_select_is_rejected() {
    assert_unsupported(
        "INSERT INTO mydb.foo SELECT * FROM other_source",
        "SELECT",
    );
}

#[test]
fn insert_or_replace_is_rejected() {
    assert_unsupported(
        "INSERT OR REPLACE INTO mydb.foo VALUES (1)",
        "OR REPLACE",
    );
}

#[test]
fn insert_or_ignore_is_rejected() {
    assert_unsupported(
        "INSERT OR IGNORE INTO mydb.foo VALUES (1)",
        "OR IGNORE",
    );
}

#[test]
fn parameterized_insert_is_rejected() {
    assert_unsupported(
        "INSERT INTO mydb.foo VALUES (?)",
        "prepared/parameterized",
    );
}

#[test]
fn dollar_parameter_insert_is_rejected() {
    assert_unsupported(
        "INSERT INTO mydb.foo VALUES ($1)",
        "prepared/parameterized",
    );
}

#[test]
fn multi_statement_intercept_is_rejected() {
    assert_unsupported(
        "SELECT 1; INSERT INTO mydb.foo VALUES (1)",
        "multi-statement",
    );
}

#[test]
fn update_with_returning_is_rejected() {
    assert_unsupported(
        "UPDATE mydb.foo SET x = 1 WHERE y = 2 RETURNING id",
        "RETURNING",
    );
}

#[test]
fn delete_with_returning_is_rejected() {
    assert_unsupported(
        "DELETE FROM mydb.foo WHERE id = 1 RETURNING *",
        "RETURNING",
    );
}

#[test]
fn parameterized_update_is_rejected() {
    assert_unsupported(
        "UPDATE mydb.foo SET x = ? WHERE id = 1",
        "prepared/parameterized",
    );
}

#[test]
fn non_attached_alias_falls_through() {
    // Not one of "our" aliases -> None -> core handles it normally.
    let att = attached_map(&["mydb"]);
    assert!(
        resolve_write_target("INSERT INTO other_db.foo VALUES (1)", &att).is_none(),
        "non-attached alias must fall through to the core"
    );
    assert!(
        resolve_write_target("UPDATE other.t SET x = 1 WHERE id = 2", &att).is_none(),
    );
    assert!(
        resolve_write_target("DELETE FROM other.t WHERE id = 1", &att).is_none(),
    );
}

#[test]
fn plain_select_falls_through() {
    let att = attached_map(&["mydb"]);
    assert!(resolve_write_target("SELECT * FROM mydb.foo", &att).is_none());
}

#[test]
fn read_only_multi_statement_falls_through() {
    // A multi-statement script that DOESN'T touch an attached alias in a
    // write position should fall through, not be rejected. Prevents the
    // intercept from ruining plain-SQL scripts that happen to contain a `;`.
    let att = attached_map(&["mydb"]);
    assert!(resolve_write_target("SELECT 1; SELECT 2", &att).is_none());
}
