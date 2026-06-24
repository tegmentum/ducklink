-- mailto extension smoke (RFC 6068 parsing).
SELECT mailto_to('mailto:alice@example.com?subject=Hello%20World&cc=bob@example.com') AS recipients;
SELECT mailto_field('mailto:alice@example.com?subject=Hello%20World&cc=bob@example.com', 'subject') AS subject;
SELECT mailto_field('mailto:alice@example.com?subject=Hello%20World&cc=bob@example.com', 'cc') AS cc;
SELECT mailto_field('mailto:alice@example.com?subject=Hello%20World&cc=bob@example.com', 'bcc') AS bcc;
SELECT mailto_to_json('mailto:alice@example.com,carol@example.com?subject=Hi&body=Yo&cc=bob@example.com') AS full;
SELECT mailto_to('https://example.com/not-a-mailto') AS nonmailto;
