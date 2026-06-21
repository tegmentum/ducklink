-- cardtype extension smoke (well-known test numbers).
SELECT card_brand('4111 1111 1111 1111') AS visa;
SELECT card_brand('5500-0000-0000-0004') AS mastercard;
SELECT card_brand('340000000000009') AS amex;
SELECT card_brand('6011000000000004') AS discover;
SELECT card_brand('3528000000000007') AS jcb;
SELECT card_brand('1234') AS unknown;
