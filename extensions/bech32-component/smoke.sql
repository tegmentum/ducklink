-- bech32 extension smoke.
SELECT bech32_decode_hex(bech32_encode('abc', 'deadbeef')) AS roundtrip;
SELECT bech32_hrp(bech32_encode('test', '00ff')) AS hrp;
SELECT bech32_valid('abc1m9h5kw3jzcs') AS maybe_valid;
SELECT bech32_valid('not bech32 at all') AS bad;
