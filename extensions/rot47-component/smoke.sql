-- rot47 extension smoke (self-inverse; "Hello" -> "w6==@").
SELECT rot47('Hello World!') AS enc;
SELECT rot47(rot47('round trip 123')) AS roundtrip;
