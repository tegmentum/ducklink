-- pintest_a probe (PLAN-prefixes v1.1 THE PIN): pin_probe() returns 111.
-- Bare + qualified both work; the qualified form disambiguates from pintest_b.
SELECT pin_probe() AS bare;
SELECT pintest_a__pin_probe() AS qualified;
