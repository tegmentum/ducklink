-- Smoke test for the `csscolor` extension (loaded by the harness). `.mode csv`.
-- Hex outputs are prefixed with 'x' because the smoke-harness expected-file
-- format treats a leading '#' as a comment (so '#ff0000' would be eaten).
SELECT 'x' || css_to_hex('red') AS red_hex;
SELECT 'x' || css_to_hex('rgb(0,128,255)') AS rgb_hex;
SELECT css_to_rgb('#ff8800') AS hex_rgb;
SELECT css_valid('rebeccapurple') AS named_ok;
SELECT css_valid('not-a-color') AS bad;
SELECT css_to_hex('not-a-color') IS NULL AS bad_is_null;
