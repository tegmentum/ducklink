-- html extension smoke: CSS-selector extraction over a small document.
SELECT html_extract('<ul><li class="x">one</li><li>two</li></ul><a href="/h">link</a>', 'li.x') AS first;
SELECT html_extract_all('<ul><li class="x">one</li><li>two</li></ul><a href="/h">link</a>', 'li') AS all;
SELECT html_attr('<ul><li class="x">one</li><li>two</li></ul><a href="/h">link</a>', 'a', 'href') AS href;
SELECT html_extract('<ul><li class="x">one</li></ul>', 'p.none') AS no_match;
