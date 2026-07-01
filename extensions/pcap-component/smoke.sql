-- pcap extension smoke: read_pcap(data BLOB) -> per-packet rows.
--
-- Fixture: a classic little-endian PCAP (Ethernet linktype) with two packets:
--   pkt 1: ts=1.000002, 4 bytes 0xDEADBEEF
--   pkt 2: ts=100.000200, 2 bytes 0xCAFE
SELECT idx, ts_sec, ts_usec, caplen, origlen, data
FROM read_pcap(unhex('d4c3b2a1020004000000000000000000ffff00000100000001000000020000000400000004000000deadbeef64000000c80000000200000002000000cafe'))
ORDER BY idx;

-- Packet count.
SELECT count(*) AS packets
FROM read_pcap(unhex('d4c3b2a1020004000000000000000000ffff00000100000001000000020000000400000004000000deadbeef64000000c80000000200000002000000cafe'));

-- Malformed blob -> zero rows (proves the function is wired, never panics).
SELECT count(*) AS bad_rows FROM read_pcap('not a pcap'::BLOB);

-- Empty blob -> zero rows.
SELECT count(*) AS empty_rows FROM read_pcap(''::BLOB);
