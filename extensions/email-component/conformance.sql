-- email extension smoke.
SELECT email_validate('a.b@example.com') AS ok;
SELECT email_validate('not-an-email') AS bad;
SELECT email_domain('user@sub.example.org') AS domain;
SELECT email_local('user.name@example.com') AS local;
SELECT email_domain('garbage') AS bad_domain;
