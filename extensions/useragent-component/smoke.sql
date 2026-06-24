-- useragent extension smoke.
-- Chrome-on-Windows UA: browser=Chrome, os contains 'Windows', not a bot.
SELECT ua_browser('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36') AS browser;
SELECT ua_browser_version('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36') AS browser_version;
SELECT ua_os('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36') AS os;
SELECT ua_category('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36') AS category;
SELECT ua_is_bot('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36') AS is_bot;
-- Googlebot UA: category=crawler, is a bot.
SELECT ua_category('Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)') AS bot_category;
SELECT ua_is_bot('Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)') AS bot_is_bot;
-- NULL passes through.
SELECT ua_browser(NULL) AS null_browser;
