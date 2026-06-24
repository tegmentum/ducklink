-- dotenv extension smoke. Input is a 3-line .env:
--   # c
--   HOST=localhost
--   PORT="5432"
SELECT dotenv_get('# c' || chr(10) || 'HOST=localhost' || chr(10) || 'PORT="5432"', 'HOST') AS host;
SELECT dotenv_get('# c' || chr(10) || 'HOST=localhost' || chr(10) || 'PORT="5432"', 'PORT') AS port;
SELECT dotenv_keys('# c' || chr(10) || 'HOST=localhost' || chr(10) || 'PORT="5432"') AS keys;
SELECT dotenv_get('# c' || chr(10) || 'HOST=localhost' || chr(10) || 'PORT="5432"', 'MISSING') AS absent;
