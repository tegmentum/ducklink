//! Hand-rolled AWS Signature Version 4 for S3 GET requests.
//!
//! The full `aws-sigv4` crate drags in the aws-smithy async runtime + http +
//! bytes machinery; for a single signed GET it's far lighter (and more portable
//! to wasm32-wasip2) to compute the four SigV4 steps directly on `hmac` +
//! `sha2`, which build cleanly for wasip2:
//!
//!   1. canonical request  = METHOD\nURI\nQUERY\nHEADERS\nSIGNED_HEADERS\nHASH
//!   2. string to sign      = "AWS4-HMAC-SHA256\n" + ts + "\n" + scope + "\n"
//!                            + sha256hex(canonical request)
//!   3. signing key         = HMAC chain over (date, region, service, "aws4_request")
//!   4. signature           = HMAC(signing key, string to sign)
//!
//! Verified offline against AWS's published "GET Object" SigV4 example (see the
//! `aws_doc_get_object_vector` test).
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// SHA-256 of an empty payload (a GET has no body). AWS documents this constant.
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// RFC 3986 unreserved set: A-Z a-z 0-9 - _ . ~ are left as-is; everything else
/// is percent-encoded. Used for the canonical query string and (with '/' kept)
/// for the canonical URI path.
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

/// Percent-encode a single path segment per RFC 3986 (AWS SigV4 rules).
fn encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for &b in seg.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// URI-encode an S3 object key into a canonical path body (each segment encoded,
/// '/' separators preserved). For S3 the canonical URI is the single-encoded key.
pub fn uri_encode_path(key: &str) -> String {
    key.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Current UTC timestamp in AWS basic format `YYYYMMDDTHHMMSSZ`.
pub fn amz_date_now() -> String {
    // wasi:clocks gives us Unix seconds via SystemTime; format manually (no chrono).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_amz_date(secs)
}

/// Format a Unix timestamp (seconds, UTC) as `YYYYMMDDTHHMMSSZ`.
pub fn format_amz_date(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let rem = unix_secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}{mo:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Compute the SigV4 `Authorization` header value.
///
/// `headers` is the full set of request headers (name lowercased). They are
/// sorted, all included as signed headers, and folded into the canonical
/// request. Returns the complete `Authorization:` value.
#[allow(clippy::too_many_arguments)]
pub fn sign_v4(
    creds: &Credentials,
    method: &str,
    region: &str,
    service: &str,
    canonical_uri: &str,
    canonical_query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    amz_date: &str,
) -> String {
    let date_stamp = &amz_date[..8]; // YYYYMMDD

    // Canonical headers: sorted by lowercased name; value trimmed.
    let mut sorted: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = sorted
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();
    let signed_headers: String = sorted
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );

    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    // Derive the signing key.
    let k_date = hmac(format!("AWS4{}", creds.secret_key).as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        creds.access_key, scope, signed_headers, signature
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_payload_constant_is_correct() {
        assert_eq!(sha256_hex(b""), EMPTY_PAYLOAD_SHA256);
    }

    #[test]
    fn formats_amz_date() {
        // 2013-05-24T00:00:00Z = 1369353600 (the AWS doc example date).
        assert_eq!(format_amz_date(1_369_353_600), "20130524T000000Z");
        // Epoch.
        assert_eq!(format_amz_date(0), "19700101T000000Z");
    }

    #[test]
    fn uri_encodes_key() {
        assert_eq!(uri_encode_path("photos/2015/sample.jpg"), "photos/2015/sample.jpg");
        assert_eq!(uri_encode_path("my key.txt"), "my%20key.txt");
        assert_eq!(uri_encode_path("a/b c/d.parquet"), "a/b%20c/d.parquet");
    }

    /// AWS's published SigV4 example: "GET Object" from the
    /// "Examples of signed requests" / "Signature Version 4 Test Suite"
    /// (Signing AWS requests, get-object). The docs give the expected
    /// signature for this exact request.
    ///
    /// Request:
    ///   GET https://examplebucket.s3.amazonaws.com/test.txt  (empty Range used in
    ///   the Range-header variant; here we use the no-range GET example which the
    ///   AWS docs present with these inputs)
    ///
    /// Access key:  AKIAIOSFODNN7EXAMPLE
    /// Secret key:  wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
    /// Region:      us-east-1, service s3
    /// Date:        20130524T000000Z
    /// Host:        examplebucket.s3.amazonaws.com
    /// Signed headers: host;range;x-amz-content-sha256;x-amz-date
    /// Range: bytes=0-9
    ///
    /// Expected (AWS docs):
    ///   Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41
    #[test]
    fn aws_doc_get_object_vector() {
        let creds = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        };
        let headers = vec![
            ("host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("range".to_string(), "bytes=0-9".to_string()),
            (
                "x-amz-content-sha256".to_string(),
                EMPTY_PAYLOAD_SHA256.to_string(),
            ),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let authz = sign_v4(
            &creds,
            "GET",
            "us-east-1",
            "s3",
            "/test.txt",
            "",
            &headers,
            EMPTY_PAYLOAD_SHA256,
            "20130524T000000Z",
        );
        assert!(
            authz.contains(
                "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            ),
            "SigV4 signature mismatch; got: {authz}"
        );
        assert!(authz.contains(
            "Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request"
        ));
        assert!(authz.contains("SignedHeaders=host;range;x-amz-content-sha256;x-amz-date"));
    }
}
