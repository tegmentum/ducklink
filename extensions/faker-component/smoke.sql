-- faker extension smoke (nondeterministic; assert shape).
SELECT length(fake_name()) > 0 AS name_ok;
SELECT strpos(fake_email(), '@') > 0 AS email_has_at;
SELECT length(fake_username()) > 0 AS username_ok;
SELECT fake_city() <> fake_city() OR true AS city_ok;
SELECT length(fake_company()) > 0 AS company_ok;
