-- xml extension smoke. Doc: <r><a>1</a><a>2</a><b>x</b></r>
SELECT xml_valid('<r><a>1</a><a>2</a><b>x</b></r>') AS ok;
SELECT xml_valid('<r>') AS bad;
SELECT xml_extract('<r><a>1</a><a>2</a><b>x</b></r>', '/r/b') AS first_b;
SELECT xml_extract_all('<r><a>1</a><a>2</a><b>x</b></r>', '/r/a') AS all_a;
SELECT xml_extract('<r><a>1</a></r>', '/r/zzz') AS missing;
