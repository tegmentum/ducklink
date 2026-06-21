-- html2text extension smoke.
SELECT html_to_text('<h1>Title</h1><p>Hello <b>bold</b> world</p>') AS stripped;
SELECT html_to_text('<a href="x">link</a> &amp; more') AS entities;
