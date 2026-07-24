//! Unit coverage for the runtime crate's pure logic: the callback registry
//! (handle allocation / lookup / removal / extension purge) and the neutral
//! `reg` model's `describe`/`summarize` rendering helpers. These are the
//! host-agnostic pieces shared by both the native bridge (Direction 2) and the
//! wasm-core host (Direction 1), so they are exercised here independently of any
//! loaded wasm component.

use ducklink_runtime::extension::{
    describe_runtime_logicaltype, summarize_extopts, summarize_funcopts,
    summarize_registration_names, summarize_runtime_columns, summarize_runtime_funcargs,
    PendingRegistrationsData,
};
use ducklink_runtime::reg;
use ducklink_runtime::{CallbackKind, CallbackRegistry};

// --- CallbackRegistry -------------------------------------------------------

#[test]
fn registry_allocates_monotonic_handles_from_one() {
    let mut reg = CallbackRegistry::new();
    let a = reg.allocate("ext", CallbackKind::Scalar, 10);
    let b = reg.allocate("ext", CallbackKind::Table, 11);
    let c = reg.allocate("ext", CallbackKind::Aggregate, 12);
    assert_eq!((a, b, c), (1, 2, 3), "new() starts handles at 1, monotonic");
}

#[test]
fn registry_get_returns_stored_entry() {
    let mut reg = CallbackRegistry::new();
    let h = reg.allocate("myext", CallbackKind::Cast, 42);
    let entry = reg.get(h).expect("allocated handle resolves");
    assert_eq!(&*entry.extension, "myext");
    assert_eq!(entry.dispatcher_handle, 42);
    assert_eq!(entry.kind, CallbackKind::Cast);
}

#[test]
fn registry_resolve_borrows_same_entry_as_get() {
    // The dispatch hot path uses `resolve` (borrowing, no clone) instead of
    // `get` (cloning). Both must agree on the stored entry.
    let mut reg = CallbackRegistry::new();
    let h = reg.allocate_quiet("myext", CallbackKind::Scalar, 7);
    let borrowed = reg.resolve(h).expect("resolve returns the stored entry");
    assert_eq!(&*borrowed.extension, "myext");
    assert_eq!(borrowed.dispatcher_handle, 7);
    assert_eq!(borrowed.kind, CallbackKind::Scalar);
    assert!(reg.resolve(h + 999).is_none(), "unknown handle resolves None");
}

#[test]
fn registry_get_unknown_handle_is_none() {
    let mut reg = CallbackRegistry::new();
    let h = reg.allocate("ext", CallbackKind::Scalar, 1);
    assert!(reg.get(h + 999).is_none());
    assert!(reg.get(0).is_none(), "handle 0 was never allocated by new()");
}

#[test]
fn registry_remove_drops_only_that_handle() {
    let mut reg = CallbackRegistry::new();
    let a = reg.allocate("ext", CallbackKind::Scalar, 1);
    let b = reg.allocate("ext", CallbackKind::Table, 2);
    reg.remove(a);
    assert!(reg.get(a).is_none(), "removed handle is gone");
    assert!(reg.get(b).is_some(), "sibling handle survives");
}

#[test]
fn registry_remove_unknown_handle_is_noop() {
    let mut reg = CallbackRegistry::new();
    let a = reg.allocate("ext", CallbackKind::Scalar, 1);
    reg.remove(a + 1234); // no panic, no effect
    assert!(reg.get(a).is_some());
}

#[test]
fn registry_handles_are_not_reused_after_remove() {
    let mut reg = CallbackRegistry::new();
    let a = reg.allocate("ext", CallbackKind::Scalar, 1);
    reg.remove(a);
    let b = reg.allocate("ext", CallbackKind::Scalar, 2);
    assert_ne!(a, b, "a freed handle is not reissued; counter keeps advancing");
    assert!(reg.get(a).is_none());
}

#[test]
fn registry_remove_extension_purges_matching_only() {
    let mut reg = CallbackRegistry::new();
    let x1 = reg.allocate("alpha", CallbackKind::Scalar, 1);
    let x2 = reg.allocate("alpha", CallbackKind::Table, 2);
    let y1 = reg.allocate("beta", CallbackKind::Aggregate, 3);
    reg.remove_extension("alpha");
    assert!(reg.get(x1).is_none());
    assert!(reg.get(x2).is_none());
    assert!(reg.get(y1).is_some(), "other extension's handles untouched");
}

#[test]
fn registry_remove_extension_unknown_is_noop() {
    let mut reg = CallbackRegistry::new();
    let a = reg.allocate("alpha", CallbackKind::Scalar, 1);
    reg.remove_extension("does-not-exist");
    assert!(reg.get(a).is_some());
}

// --- describe() renderings --------------------------------------------------

#[test]
fn callback_kind_describe_covers_all_variants() {
    assert_eq!(CallbackKind::Scalar.describe(), "scalar");
    assert_eq!(CallbackKind::Table.describe(), "table");
    assert_eq!(CallbackKind::Aggregate.describe(), "aggregate");
    assert_eq!(CallbackKind::Pragma.describe(), "pragma");
    assert_eq!(CallbackKind::Cast.describe(), "cast");
}

#[test]
fn logical_type_describe_covers_all_variants() {
    use reg::LogicalType::*;
    assert_eq!(Boolean.describe(), "BOOLEAN");
    assert_eq!(Int64.describe(), "INT64");
    assert_eq!(Uint64.describe(), "UINT64");
    assert_eq!(Float64.describe(), "FLOAT64");
    assert_eq!(Text.describe(), "TEXT");
    assert_eq!(Blob.describe(), "BLOB");
}

#[test]
fn func_flags_describe_none_single_and_multiple() {
    assert_eq!(reg::FuncFlags::default().describe(), "none");

    let one = reg::FuncFlags {
        deterministic: true,
        ..Default::default()
    };
    assert_eq!(one.describe(), "[deterministic]");

    let many = reg::FuncFlags {
        deterministic: true,
        commutative: true,
        stateless: true,
        side_effecting: true,
        deprecated: true,
    };
    assert_eq!(
        many.describe(),
        "[deterministic, commutative, stateless, sideeffecting, deprecated]"
    );
}

#[test]
fn describe_runtime_logicaltype_delegates_to_describe() {
    assert_eq!(describe_runtime_logicaltype(&reg::LogicalType::Text), "TEXT");
    assert_eq!(
        describe_runtime_logicaltype(&reg::LogicalType::Blob),
        "BLOB"
    );
}

// --- summarize helpers ------------------------------------------------------

#[test]
fn summarize_registration_names_empty_preview_and_overflow() {
    let none: Vec<String> = Vec::new();
    assert_eq!(summarize_registration_names(&none, |s| s.as_str()), "none");

    let three = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    assert_eq!(
        summarize_registration_names(&three, |s| s.as_str()),
        "a, b, c"
    );

    let five = vec![
        "a".to_string(),
        "b".to_string(),
        "c".to_string(),
        "d".to_string(),
        "e".to_string(),
    ];
    assert_eq!(
        summarize_registration_names(&five, |s| s.as_str()),
        "a, b, c, +2 more"
    );
}

#[test]
fn summarize_runtime_funcargs_named_and_anonymous() {
    assert_eq!(summarize_runtime_funcargs(&[]), "[]");

    let args = vec![
        reg::FuncArg {
            name: Some("x".to_string()),
            logical: reg::LogicalType::Int64,
        },
        reg::FuncArg {
            name: None,
            logical: reg::LogicalType::Text,
        },
    ];
    assert_eq!(summarize_runtime_funcargs(&args), "[x:INT64, -:TEXT]");
}

#[test]
fn summarize_runtime_columns_renders_name_and_type() {
    assert_eq!(summarize_runtime_columns(&[]), "[]");

    let cols = vec![
        reg::ColumnDef {
            name: "id".to_string(),
            logical: reg::LogicalType::Int64,
        },
        reg::ColumnDef {
            name: "label".to_string(),
            logical: reg::LogicalType::Text,
        },
    ];
    assert_eq!(
        summarize_runtime_columns(&cols),
        "[id:INT64, label:TEXT]"
    );
}

#[test]
fn summarize_funcopts_none_and_full() {
    assert_eq!(summarize_funcopts(None), "none");

    let opts = reg::FuncOpts {
        description: Some("hash a value".to_string()),
        tags: vec!["crypto".to_string(), "hash".to_string()],
        attributes: reg::FuncFlags {
            deterministic: true,
            ..Default::default()
        },
    };
    assert_eq!(
        summarize_funcopts(Some(&opts)),
        "description='hash a value', tags=[crypto, hash], attrs=[deterministic]"
    );

    let bare = reg::FuncOpts {
        description: None,
        tags: vec![],
        attributes: reg::FuncFlags::default(),
    };
    assert_eq!(
        summarize_funcopts(Some(&bare)),
        "description='-', tags=none, attrs=none"
    );
}

#[test]
fn summarize_extopts_none_and_full() {
    assert_eq!(summarize_extopts(None), "none");

    let opts = reg::ExtOpts {
        description: Some("read a file".to_string()),
        tags: vec!["io".to_string()],
    };
    assert_eq!(
        summarize_extopts(Some(&opts)),
        "description='read a file', tags=[io]"
    );

    let bare = reg::ExtOpts {
        description: None,
        tags: vec![],
    };
    assert_eq!(summarize_extopts(Some(&bare)), "description='-', tags=none");
}

// --- PendingRegistrationsData::append --------------------------------------

fn scalar(name: &str) -> reg::ScalarReg {
    reg::ScalarReg {
        extension: "ext".to_string(),
        name: name.to_string(),
        arguments: vec![],
        returns: reg::LogicalType::Int64,
        callback_handle: 1,
        options: None,
    }
}

#[test]
fn pending_append_merges_all_kinds_and_drains_other() {
    let mut base = PendingRegistrationsData::default();
    base.scalars.push(scalar("first"));

    let mut other = PendingRegistrationsData::default();
    other.scalars.push(scalar("second"));
    other.tables.push(reg::TableReg {
        extension: "ext".to_string(),
        name: "t".to_string(),
        arguments: vec![],
        columns: vec![],
        callback_handle: 2,
        options: None,
    });
    other.macros.push(reg::MacroReg {
        extension: "ext".to_string(),
        schema: "main".to_string(),
        name: "m".to_string(),
        parameters: vec![],
        definition_sql: "SELECT 1".to_string(),
    });
    other.casts.push(reg::CastReg {
        extension: "ext".to_string(),
        source: "a".to_string(),
        target: "b".to_string(),
        callback_handle: 3,
        implicit_cost: None,
    });

    base.append(other);

    assert_eq!(base.scalars.len(), 2, "scalars concatenated in order");
    assert_eq!(base.scalars[0].name, "first");
    assert_eq!(base.scalars[1].name, "second");
    assert_eq!(base.tables.len(), 1);
    assert_eq!(base.macros.len(), 1);
    assert_eq!(base.casts.len(), 1);
    assert_eq!(base.aggregates.len(), 0);
}

// --- T2-4: CastReg.implicit_cost round-trip --------------------------------
//
// The three legal shapes the WIT `option<s32>` field can carry once captured
// into a `reg::CastReg` are: `None` (unset → ducklink applies its 100
// default at native-registration time), `Some(v)` with `v >= 0` (positive
// implicit cost forwarded verbatim), and `Some(-1)` (the DuckDB C API's
// "explicit-only" convention; the consolidator skips the setter). Sweep 4
// added the `Some(50)` / `Some(-1)` coverage — previously only the `None`
// arm was exercised in `pending_append_merges_all_kinds_and_drains_other`.

fn cast_reg_with_cost(cost: Option<i32>) -> reg::CastReg {
    reg::CastReg {
        extension: "ext".to_string(),
        source: "src".to_string(),
        target: "tgt".to_string(),
        callback_handle: 1,
        implicit_cost: cost,
    }
}

#[test]
fn cast_reg_carries_implicit_cost_none() {
    let c = cast_reg_with_cost(None);
    assert_eq!(c.implicit_cost, None);
}

#[test]
fn cast_reg_carries_implicit_cost_some_positive() {
    let c = cast_reg_with_cost(Some(50));
    assert_eq!(c.implicit_cost, Some(50));
}

#[test]
fn cast_reg_carries_implicit_cost_some_explicit_only() {
    let c = cast_reg_with_cost(Some(-1));
    assert_eq!(c.implicit_cost, Some(-1));
}
