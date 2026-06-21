-- phonenumber extension smoke (US +1 415 555 2671).
SELECT phone_valid('+14155552671', '') AS valid_e164;
SELECT phone_valid('(415) 555-2671', 'US') AS valid_national;
SELECT phone_valid('+1123', '') AS invalid;
SELECT phone_format('4155552671', 'US', 'e164') AS e164;
SELECT phone_format('+442083661177', '', 'international') AS uk_intl;
SELECT phone_country_code('+442083661177', '') AS uk_code;
