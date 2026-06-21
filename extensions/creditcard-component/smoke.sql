-- Smoke test for the `creditcard` extension (loaded by the harness). `.mode csv`
-- emits a header (the AS alias) then the value. Standard test PANs.
SELECT cc_validate('4111 1111 1111 1111') AS visa_ok;
SELECT cc_validate('5500 0000 0000 0004') AS mc_ok;
SELECT cc_validate('3400 0000 0000 009') AS amex_ok;
SELECT cc_validate('4111 1111 1111 1112') AS bad_luhn;
SELECT cc_network('4111111111111111') AS visa_net;
SELECT cc_network('5105105105105100') AS mc_net;
SELECT cc_network('340000000000009') AS amex_net;
SELECT cc_network('6011000990139424') AS discover_net;
