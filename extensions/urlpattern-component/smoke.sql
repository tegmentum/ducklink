-- urlpattern extension smoke (WHATWG URLPattern matching).
SELECT url_pattern_test('https://example.com/books/:id', 'https://example.com/books/123') AS match;
SELECT url_pattern_test('https://example.com/books/:id', 'https://example.com/movies/123') AS nomatch;
SELECT url_pattern_match('https://example.com/books/:id', 'https://example.com/books/123') AS groups;
SELECT url_pattern_test('https://example.com/(', 'https://example.com/books/123') AS bad;
