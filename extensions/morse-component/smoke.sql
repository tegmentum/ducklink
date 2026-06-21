-- morse extension smoke.
SELECT morse_encode('SOS') AS sos;
SELECT morse_encode('Hello World') AS hw;
SELECT morse_decode('... --- ...') AS dec_sos;
SELECT morse_decode('.... .. / - .... . .-. .') AS dec_phrase;
