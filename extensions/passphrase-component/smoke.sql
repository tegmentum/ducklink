-- passphrase extension smoke (nondeterministic; assert shape).
SELECT length(passphrase()) > 0 AS nonempty;
SELECT passphrase() <> passphrase() AS distinct_each;
SELECT array_length(string_split(passphrase(), ' ')) >= 3 AS multiword;
