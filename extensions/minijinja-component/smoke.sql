-- minijinja extension smoke: Jinja2-style templating over a JSON context.
SELECT jinja_render('Hello {{ name }}!', '{"name": "World"}') AS greeting;
SELECT jinja_render('{% for n in nums %}{{ n }}{% endfor %}', '{"nums": [1, 2, 3]}') AS loop;
SELECT jinja_render('{{ word | upper }}', '{"word": "hi"}') AS filter;
-- Invalid template -> NULL.
SELECT jinja_render('Hello {{ name ', '{}') AS bad_tmpl;
-- Malformed JSON context -> NULL.
SELECT jinja_render('Hello {{ name }}', 'not json') AS bad_json;
-- jinja_valid reports compile status.
SELECT jinja_valid('Hello {{ name }}!') AS ok;
SELECT jinja_valid('Hello {{ name ') AS broken;
