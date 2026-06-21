-- bbcode extension smoke.
SELECT bbcode_to_html('[b]bold[/b] and [i]italic[/i]') AS basic;
SELECT bbcode_to_html('[url=https://x.com]link[/url]') AS url;
SELECT bbcode_to_html('[code]x=1[/code]') AS code;
SELECT bbcode_to_html('plain text') AS plain;
