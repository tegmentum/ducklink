-- escape extension smoke.
SELECT html_escape('<a href="x">Tom & Jerry</a>') AS h;
SELECT html_unescape('Tom &amp; Jerry &lt;3') AS u;
SELECT url_encode('a b/c?d=1') AS e;
SELECT url_decode('a%20b%2Fc') AS d;
