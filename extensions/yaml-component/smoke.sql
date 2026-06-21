-- yaml extension smoke (multiline built with chr(10)).
SELECT yaml_to_json('name: Alice' || chr(10) || 'age: 30') AS j;
SELECT yaml_to_json('[1, 2, 3]') AS arr;
SELECT yaml_to_json(': : invalid : :') AS bad;
