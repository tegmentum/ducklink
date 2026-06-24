-- magic extension smoke: feed known magic-byte prefixes as BLOB literals and
-- assert sniffed mime / extension / matcher class. Unknown bytes -> NULL.

-- PNG signature (89 50 4E 47 0D 0A 1A 0A). DuckDB BLOB literals only accept
-- \xNN hex escapes, so the full 8-byte magic is written in hex.
SELECT magic_mime('\x89\x50\x4E\x47\x0D\x0A\x1A\x0A'::BLOB) AS png_mime;
SELECT magic_extension('\x89\x50\x4E\x47\x0D\x0A\x1A\x0A'::BLOB) AS png_ext;
SELECT magic_matcher_type('\x89\x50\x4E\x47\x0D\x0A\x1A\x0A'::BLOB) AS png_class;
SELECT is_image('\x89\x50\x4E\x47\x0D\x0A\x1A\x0A'::BLOB) AS png_is_image;

-- PDF
SELECT magic_mime('%PDF-1.4'::BLOB) AS pdf_mime;
SELECT magic_extension('%PDF-1.4'::BLOB) AS pdf_ext;
SELECT magic_matcher_type('%PDF-1.4'::BLOB) AS pdf_class;
SELECT is_image('%PDF-1.4'::BLOB) AS pdf_is_image;

-- GIF
SELECT magic_mime('GIF89a'::BLOB) AS gif_mime;
SELECT magic_extension('GIF89a'::BLOB) AS gif_ext;

-- JPEG (FF D8 FF E0 ... JFIF)
SELECT magic_mime('\xFF\xD8\xFF\xE0'::BLOB) AS jpg_mime;
SELECT magic_extension('\xFF\xD8\xFF\xE0'::BLOB) AS jpg_ext;

-- Unknown bytes -> NULL
SELECT magic_mime('not magic at all'::BLOB) AS unknown_mime;
SELECT is_image('not magic at all'::BLOB) AS unknown_is_image;
