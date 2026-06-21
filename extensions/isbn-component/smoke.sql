-- isbn extension smoke.
SELECT isbn_valid('0-306-40615-2') AS ten_ok;
SELECT isbn_valid('978-0-306-40615-7') AS thirteen_ok;
SELECT isbn_valid('0-306-40615-3') AS bad_checkdigit;
SELECT isbn_normalize('0 306 40615 2') AS norm;
SELECT isbn_normalize('not-an-isbn') AS bad_norm;
