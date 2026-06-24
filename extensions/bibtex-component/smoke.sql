-- bibtex extension smoke (one tiny inline @article entry).
SELECT bibtex_count('@article{smith2020, title={Hello}, author={Smith}, year={2020}}') AS cnt;
SELECT bibtex_keys('@article{smith2020, title={Hello}, author={Smith}, year={2020}}') AS keys;
SELECT bibtex_count('not a bib entry @@@') AS bad_count;
SELECT bibtex_keys(NULL) AS null_keys;
