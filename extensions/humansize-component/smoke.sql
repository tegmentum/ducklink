-- humansize extension smoke.
SELECT humansize(1500000) AS dec_mb;
SELECT humansize_binary(1048576) AS one_mib;
SELECT humansize(0) AS zero;
SELECT humansize(999) AS bytes;
