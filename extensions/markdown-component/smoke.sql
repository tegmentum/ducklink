-- Smoke test for the `markdown` extension (loaded by the harness). `.mode csv`.
SELECT md_to_html('# Hi') AS h1;
SELECT md_to_html('a **b** c') AS bold;
SELECT md_to_text('# Title

some **bold** text') AS plain;
SELECT md_to_html(NULL) AS null_in;
