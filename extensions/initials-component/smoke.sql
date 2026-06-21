-- initials extension smoke.
SELECT initials('Portable Document Format') AS pdf;
SELECT initials('the quick brown fox') AS tqbf;
SELECT initials_dotted('Thomas Stearns Eliot') AS tse;
SELECT initials('   ') AS empty;
