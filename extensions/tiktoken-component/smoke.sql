-- tiktoken extension smoke ("hello world" is 2 tokens in cl100k/o200k).
SELECT tiktoken_count('hello world', 'cl100k_base') AS cl100k_n;
SELECT tiktoken_count('hello world', 'o200k_base') AS o200k_n;
SELECT tiktoken_encode('hello world', 'cl100k_base') AS ids;
SELECT tiktoken_decode('[15339, 1917]', 'cl100k_base') AS back;
SELECT tiktoken_decode(tiktoken_encode('round trip test', 'o200k_base'), 'o200k_base') AS roundtrip;
SELECT tiktoken_count('default encoding', '') AS defaulted;
