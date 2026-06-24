-- rhai extension smoke: evaluate Rhai expressions as SQL scalars.
SELECT rhai_eval('40 + 2') AS sum;
SELECT rhai_eval('"a" + "b"') AS concat;
SELECT rhai_eval('let x = 5; x * x') AS sq;
SELECT rhai_eval_int('6 * 7') AS prod;
SELECT rhai_eval('1/0') AS divzero;
