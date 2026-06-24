-- jaq (jq-style JSON query) extension smoke.
SELECT jq('{"a":1,"b":2}', '.a') AS a;
SELECT jq('[1,2,3]', 'map(.*2)') AS doubled;
SELECT jq('{"x":[1,2]}', '.x[]') AS multi;
SELECT jq_first('{"x":[1,2]}', '.x[]') AS first;
SELECT jq('x', '.') AS bad_json;
