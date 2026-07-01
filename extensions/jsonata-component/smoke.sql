-- jsonata extension smoke: evaluate JSONata over a JSON document.
-- Path navigation.
SELECT jsonata('a.b', '{"a":{"b":42}}') AS nav;
-- Aggregate over a path.
SELECT jsonata('$sum(items.price)', '{"items":[{"price":10},{"price":20}]}') AS total;
-- Predicate filter projecting a field.
SELECT jsonata('orders[price > 100].product',
               '{"orders":[{"product":"Laptop","price":1200},{"product":"Mouse","price":25}]}') AS pick;
-- Invalid expression -> NULL.
SELECT jsonata('bad syntax (', '{}') AS bad_expr;
-- Malformed JSON -> NULL.
SELECT jsonata('a', 'not json') AS bad_json;
