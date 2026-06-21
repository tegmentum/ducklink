-- emoji extension smoke.
SELECT emoji_name('🚀') AS rocket;
SELECT emoji_shortcode('😀') AS grin;
SELECT emoji_char('rocket') AS from_code;
SELECT emoji_char(':tada:') AS tada;
SELECT emoji_name('notanemoji') AS bad;
