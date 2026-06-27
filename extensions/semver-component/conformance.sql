-- semver extension smoke.
SELECT semver_valid('1.2.3') AS ok;
SELECT semver_valid('not.a.version') AS bad;
SELECT semver_major('2.5.9') AS major;
SELECT semver_minor('2.5.9') AS minor;
SELECT semver_patch('2.5.9') AS patch;
SELECT semver_compare('1.0.0', '1.0.1') AS lt;
SELECT semver_compare('2.0.0', '2.0.0') AS eq;
SELECT semver_compare('3.1.0', '3.0.9') AS gt;
