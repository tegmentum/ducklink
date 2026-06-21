-- Smoke test for the `case` extension (loaded by the harness). `.mode csv`.
SELECT to_snake_case('fooBarBaz') AS snake;
SELECT to_camel_case('foo_bar_baz') AS camel;
SELECT to_pascal_case('foo-bar') AS pascal;
SELECT to_kebab_case('FooBarBaz') AS kebab;
SELECT to_title_case('foo_bar') AS title;
SELECT to_constant_case('fooBar') AS constant;
SELECT to_snake_case(NULL) AS null_in;
