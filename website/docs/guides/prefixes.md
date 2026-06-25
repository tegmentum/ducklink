---
id: prefixes
title: Function prefixes
sidebar_label: Function prefixes
---

# Function prefixes — SPARQL-style namespacing

The large component catalog makes SQL-function name collisions inevitable.
Prefixes replace ad-hoc renames with principled, SPARQL-style namespacing:
`jsonfns__json_valid(...)` is always available and unambiguous, while bare
`json_valid(...)` keeps working. Operators can **see** collisions and **pin** which
implementation wins the bare name.

## The problem ducklink has already lived

The catalog has hit name collisions repeatedly with lossy workarounds:

- **`http` → `httpclient`, `sqlite` → `sqlitewasm`** — extensions renamed because
  the name collided with an embedded official extension the core resolved first.
- **The "no-overlap-with-builtins" rule** — every component author must manually
  check each function name against DuckDB's builtins.
- **The [de-embed components](../architecture/lean-core.md) register official
  names** — `jsonfns` registers `json_valid`/…, `inetfns` registers `host`/…,
  `spatialfns` registers `ST_*`. Two such components could collide.

## Naming & separator

The feature is **prefixes**, not "namespaces" ("namespace" in SQL already means a
catalog/schema). The separator is `__` (double underscore): DuckDB unquoted
identifiers are `[A-Za-z_][A-Za-z0-9_]*`, so `prefix__name(...)` needs no quoting
— unlike `:` (SPARQL-canonical, requires quotes) or `.` (collides with DuckDB's
`schema.function` syntax). Case-insensitive folding applies to both parts.

## Identity: the expansion, not the short prefix

Each prefix has a `name` (short, used in SQL) plus an `expansion` (an opaque
global-identity token — a URL, `com.tegmentum.ducklink`, a URN, any string).
ducklink does **not** validate the expansion format. Function identity is
`(expansion, function_name, n_args)`; the short prefix is a per-database alias.

## The key DuckDB adaptation

Unlike SQLite (silent last-wins via `sqlite3_create_function`),
`duckdb_register_scalar_function` can **return an error** on a conflicting
registration rather than silently shadowing. So bare-name-on-collision can't be
"let the engine shadow it" — ducklink must explicitly decide and effect the
bare-name owner.

The shipped mechanism (v1.1, host-only, no core rebuild) is a **wrapper macro**:
the host retains every bare registration def keyed by `(name, shape, n_args,
expansion)`, and the pin is effected with
`CREATE OR REPLACE MACRO {name}(args) AS ({prefix}__{name}(args))` against the
**always-registered** qualified form. The wrapper macro shadows any later
bare-scalar registration, so the pin is **load-order independent for free** —
loading a new impl can't steal the bare name.

## Storage

Three per-database tables travel with the `.duckdb` file (created on first
`.prefix` use): `__ducklink_prefix` (the alias → expansion map),
`__ducklink_prefix_function` (registrations keyed by `(expansion, function_name,
shape, n_args)`), and `__ducklink_prefix_pin` (the operator pin). The `shape`
column distinguishes scalar / table / aggregate / collation / pragma / macro / …
because ducklink has more registration shapes than SQLite.

## Surface — the `.prefix` dot commands

Modeled on `core-dotcmd` + `bundle-dotcmd`, in the `prefix-dotcmd` extension:

```
.prefix add foaf http://xmlns.com/foaf/0.1/ "Friend of a friend"
.prefix add tegmentum com.tegmentum.ducklink
.prefix list                      -- name | expansion | description | last_used
.prefix functions foaf            -- functions under foaf's expansion
.prefix expansion foaf
.prefix rename foaf bar
.prefix modify foaf "Updated description"
.prefix delete foaf               -- alias row only; expansion-keyed functions persist
.prefix prefer json_valid jsonfns -- pin bare-name owner; writes _prefix_pin
.prefix unprefer json_valid
.prefix conflicts                 -- current bare-name ambiguities + owner + pin
.prefix verify                    -- stale-entry check vs loaded components
```

All commands run through the dotcmd `spi.query` against the per-db prefix tables.

## What it is / is not

It adds always-available `prefix__name` qualified forms, load-time collision
warnings + a `.prefix conflicts` view, and an optional `.prefix prefer` pin. It
does **not** change which impl bare `name()` hits beyond the operator pin, does
**not** error on ambiguity at call time, and does **not** require users to update
existing SQL. Strictly additive: `SELECT json_valid(...)` keeps working exactly as
before, regardless of how many components load or collide.

## Status

- **v1.1 — DONE (host-only, no core rebuild).** The pin (`.prefix
  prefer`/`unprefer`) is real and load-order independent via the wrapper macro.
  Qualified forms exist for scalar / table / aggregate **and** the remaining
  name-keyed shapes (collation, pragma, macro). Probe components
  (`pintest_a`/`pintest_b`, registering the same bare `pin_probe()` → 111 / 222
  under distinct expansions) make the flip visible; `make smoke-dotcmd prefix`
  runs it end-to-end.
- **Out of scope (deliberately).** CAST / STORAGE / FILES / INDEX shapes stay
  unprefixed — they are keyed by `(from_type, to_type)` / an ATTACH TYPE name / a
  URL scheme / an index-type name, a different collision surface with no
  `prefix__name` call site.

No new WIT capability is required — the registration wrapper lives in the existing
host forwarding path, and `.prefix` uses the dotcmd `spi` that already exists.
