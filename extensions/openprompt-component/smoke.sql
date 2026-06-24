-- openprompt extension smoke (network-gated; OpenAI-compatible chat completions).
--
-- A live completion is nondeterministic and needs a real key + network, so this
-- smoke asserts only the deterministic OFFLINE / no-key path: with OPENAI_API_KEY
-- unset, both scalars are registered and the missing-key branch returns NULL
-- cleanly (no panic, no error). This proves the functions exist and the unhappy
-- path is clean.
--
-- LIVE test (run by hand): export OPENAI_API_KEY=sk-...  (optionally
-- OPENAI_BASE_URL / OPENAI_MODEL), grant the network capability, then:
--     DUCKLINK_NETWORK_GRANT=openprompt python3 tooling/smoke.py openprompt
-- and replace the assertions below with e.g.
--     SELECT length(prompt('Say hi in one word.')) > 0;
SELECT prompt('hi') IS NULL AS no_key_prompt;
SELECT prompt_model('hi', 'gpt-4o-mini') IS NULL AS no_key_prompt_model;
SELECT prompt(NULL) IS NULL AS null_arg;
