-- isbnconv extension smoke (0-306-40615-2 <-> 978-0-306-40615-7).
SELECT isbn10_to_13('0306406152') AS to13;
SELECT isbn13_to_10('9780306406157') AS to10;
SELECT isbn13_to_10('9790306406157') AS not_978;
SELECT isbn10_to_13('123') AS bad;
