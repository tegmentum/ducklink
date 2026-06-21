-- idna extension smoke.
SELECT idna_to_ascii('münchen.de') AS a;
SELECT idna_to_unicode('xn--mnchen-3ya.de') AS u;
SELECT idna_to_ascii('пример.рф') AS cyr;
