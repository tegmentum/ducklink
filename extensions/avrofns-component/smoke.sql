-- avrofns smoke: read the READ side of DuckDB's avro extension from an in-memory
-- Avro Object Container File (OCF) BLOB.
--
-- Fixture: a {a: long, b: string} record schema with 2 records (a=[1,2],
-- b=["x","y"]), written by apache-avro and captured as the hex string below.
-- The wasm core passes table-function BLOB params as VARCHAR hex, which the
-- component decodes back to OCF bytes via from_hex on the host path / its own
-- hex_decode otherwise.
-- avro_schema -> the two top-level fields and their Avro types.
SELECT field, type FROM avro_schema(from_hex('4f626a0104166176726f2e736368656d61be017b2274797065223a227265636f7264222c226e616d65223a2274222c226669656c6473223a5b7b226e616d65223a2261222c2274797065223a226c6f6e67227d2c7b226e616d65223a2262222c2274797065223a22737472696e67227d5d7d146176726f2e636f646563086e756c6c00b15f8153452da3f6cee3111499eb866f040c020278040279b15f8153452da3f6cee3111499eb866f')) ORDER BY field;
-- read_avro (MELTED) -> one (row_no, col, val) tuple per record field.
SELECT row_no, col, val FROM read_avro(from_hex('4f626a0104166176726f2e736368656d61be017b2274797065223a227265636f7264222c226e616d65223a2274222c226669656c6473223a5b7b226e616d65223a2261222c2274797065223a226c6f6e67227d2c7b226e616d65223a2262222c2274797065223a22737472696e67227d5d7d146176726f2e636f646563086e756c6c00b15f8153452da3f6cee3111499eb866f040c020278040279b15f8153452da3f6cee3111499eb866f')) ORDER BY row_no, col;
-- Malformed blob -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS bad_rows FROM read_avro('not avro'::BLOB);
-- Empty blob -> zero rows.
SELECT count(*) AS empty_rows FROM read_avro(''::BLOB);
