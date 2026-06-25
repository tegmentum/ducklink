-- pintest_b probe (PLAN-prefixes v1.1 THE PIN): pin_probe() returns 222.
-- Bare + qualified both work; the qualified form disambiguates from pintest_a.
SELECT pin_probe() AS bare;
SELECT pintest_b__pin_probe() AS qualified;
