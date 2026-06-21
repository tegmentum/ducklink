-- quotedprintable extension smoke.
SELECT qp_encode('Café = good') AS enc;
SELECT qp_decode('Caf=C3=A9 =3D good') AS dec;
SELECT qp_decode(qp_encode('round trip ñ')) AS roundtrip;
