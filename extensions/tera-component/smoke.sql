-- tera extension smoke: Jinja2/Django-style templating over a JSON context.
SELECT tera_render('Hello {{ name }}!', '{"name": "World"}') AS greeting;
SELECT tera_render('{% for n in nums %}{{ n }}{% endfor %}', '{"nums": [1, 2, 3]}') AS loop;
SELECT tera_render('{{ word | upper }}', '{"word": "hi"}') AS filter;
-- Invalid template -> NULL.
SELECT tera_render('Hello {{ name ', '{}') AS bad_tmpl;
-- Malformed JSON context -> NULL.
SELECT tera_render('Hello {{ name }}', 'not json') AS bad_json;
-- tera_valid reports compile status.
SELECT tera_valid('Hello {{ name }}!') AS ok;
SELECT tera_valid('Hello {{ name ') AS broken;
