//! Pure (network-free) Azure Blob request construction: az:// URL parsing,
//! credential resolution (env vars + connection string), the blob HTTPS URL,
//! SAS-token query append, and Shared Key (HMAC-SHA256) request signing.
//!
//! Everything here is `std`-only and deterministic so it can be unit-tested
//! natively without Azure or the wasm host (see the `#[cfg(test)]` module).

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Azure Blob default endpoint suffix.
pub const BLOB_SUFFIX: &str = "blob.core.windows.net";

/// A parsed `az://` (or `azure://`) reference: container + blob path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzPath {
    /// Optional account baked into the URL (az://account.container/blob is NOT
    /// standard; DuckDB uses az://container/blob with the account supplied out
    /// of band). We only ever fill this from credentials, never the URL.
    pub container: String,
    pub blob: String,
}

/// Parse `az://container/blob/path` (also accepts `azure://`). The first path
/// segment is the container; the remainder (may contain '/') is the blob.
pub fn parse_az_url(url: &str) -> Result<AzPath, String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("az://")
        .or_else(|| url.strip_prefix("azure://"))
        .ok_or_else(|| format!("azfs: not an az:// url: '{url}'"))?;
    let rest = rest.trim_start_matches('/');
    let (container, blob) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    if container.is_empty() {
        return Err(format!("azfs: empty container in '{url}'"));
    }
    if blob.is_empty() {
        return Err(format!("azfs: empty blob path in '{url}'"));
    }
    Ok(AzPath {
        container: container.to_string(),
        blob: blob.to_string(),
    })
}

/// Resolved credentials for reaching a storage account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub account: Option<String>,
    /// SAS token (with or without a leading '?'). Preferred auth path.
    pub sas_token: Option<String>,
    /// Base64-encoded Shared Key (account key) for HMAC signing.
    pub account_key: Option<String>,
    /// Optional explicit blob endpoint suffix (defaults to BLOB_SUFFIX).
    pub endpoint_suffix: Option<String>,
}

impl Credentials {
    /// Merge `other` into self, filling only fields we don't already have.
    fn fill_from(&mut self, other: Credentials) {
        if self.account.is_none() {
            self.account = other.account;
        }
        if self.sas_token.is_none() {
            self.sas_token = other.sas_token;
        }
        if self.account_key.is_none() {
            self.account_key = other.account_key;
        }
        if self.endpoint_suffix.is_none() {
            self.endpoint_suffix = other.endpoint_suffix;
        }
    }
}

/// Parse an Azure Storage connection string:
///   "DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=base64==;..."
///   or one carrying "SharedAccessSignature=sv=...&sig=..."
pub fn parse_connection_string(cs: &str) -> Credentials {
    let mut creds = Credentials::default();
    for part in cs.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            // AccountKey base64 contains '=' padding; split_once keeps the rest.
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        match k.to_ascii_lowercase().as_str() {
            "accountname" => creds.account = Some(v.to_string()),
            "accountkey" => creds.account_key = Some(v.to_string()),
            "sharedaccesssignature" => creds.sas_token = Some(v.to_string()),
            "endpointsuffix" => creds.endpoint_suffix = Some(v.to_string()),
            _ => {}
        }
    }
    creds
}

/// Resolve credentials from a connection-string override and a getenv closure.
/// Precedence: explicit connection string, then individual env vars.
/// Env vars (matching the official azure extension / Azure SDK conventions):
///   AZURE_STORAGE_CONNECTION_STRING
///   AZURE_STORAGE_ACCOUNT
///   AZURE_STORAGE_SAS_TOKEN
///   AZURE_STORAGE_KEY  (a.k.a. account key, base64)
pub fn resolve_credentials<F>(getenv: F) -> Credentials
where
    F: Fn(&str) -> Option<String>,
{
    let mut creds = Credentials::default();

    if let Some(cs) = getenv("AZURE_STORAGE_CONNECTION_STRING") {
        if !cs.trim().is_empty() {
            creds.fill_from(parse_connection_string(&cs));
        }
    }
    let env_creds = Credentials {
        account: getenv("AZURE_STORAGE_ACCOUNT").filter(|s| !s.trim().is_empty()),
        sas_token: getenv("AZURE_STORAGE_SAS_TOKEN").filter(|s| !s.trim().is_empty()),
        account_key: getenv("AZURE_STORAGE_KEY").filter(|s| !s.trim().is_empty()),
        endpoint_suffix: getenv("AZURE_STORAGE_ENDPOINT_SUFFIX")
            .filter(|s| !s.trim().is_empty()),
    };
    creds.fill_from(env_creds);
    creds
}

/// The host + path components for a blob, given creds + parsed az path.
/// Returns (host, "/container/blob").
pub fn blob_host_and_path(creds: &Credentials, p: &AzPath) -> Result<(String, String), String> {
    let account = creds
        .account
        .as_deref()
        .ok_or_else(|| "azfs: no storage account (set AZURE_STORAGE_ACCOUNT or a connection string)".to_string())?;
    let suffix = creds.endpoint_suffix.as_deref().unwrap_or(BLOB_SUFFIX);
    let host = format!("{account}.{suffix}");
    let path = format!("/{}/{}", p.container, p.blob);
    Ok((host, path))
}

/// Build the full https:// blob URL with the SAS token appended (no signing).
/// This is the PRIMARY auth path.
pub fn build_sas_url(host: &str, path: &str, sas_token: &str) -> String {
    let sas = sas_token.trim().trim_start_matches('?');
    format!("https://{host}{path}?{sas}")
}

/// An outgoing signed GET request: the URL plus the headers to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRequest {
    pub url: String,
    /// Header lines as (name, value), already including x-ms-date / x-ms-version
    /// and the computed Authorization.
    pub headers: Vec<(String, String)>,
}

/// Compute the Shared Key `Authorization` header for a GET of `host``path`.
/// `x_ms_date` is an RFC1123 GMT date string (caller supplies it so signing is
/// deterministic/testable). `account` is the storage account name; `key_b64` is
/// the base64-encoded account key.
///
/// String-to-sign for Shared Key (blob/queue), GET with no body:
///   VERB\n
///   Content-Encoding\n Content-Language\n Content-Length\n Content-MD5\n
///   Content-Type\n Date\n If-Modified-Since\n If-Match\n If-None-Match\n
///   If-Unmodified-Since\n Range\n
///   CanonicalizedHeaders
///   CanonicalizedResource
/// We send no conditional/content headers and rely on x-ms-date (Date left
/// blank), so most lines are empty.
pub fn sign_shared_key(
    account: &str,
    key_b64: &str,
    host: &str,
    path: &str,
    x_ms_date: &str,
    x_ms_version: &str,
) -> Result<SignedRequest, String> {
    let key = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .map_err(|e| format!("azfs: account key is not valid base64: {e}"))?;

    // Canonicalized headers: all x-ms-* headers, lowercased, sorted by name,
    // "name:value" joined by '\n', trailing '\n'.
    let mut x_ms_headers = vec![
        ("x-ms-date".to_string(), x_ms_date.to_string()),
        ("x-ms-version".to_string(), x_ms_version.to_string()),
    ];
    x_ms_headers.sort_by(|a, b| a.0.cmp(&b.0));
    let mut canon_headers = String::new();
    for (k, v) in &x_ms_headers {
        canon_headers.push_str(k);
        canon_headers.push(':');
        canon_headers.push_str(v.trim());
        canon_headers.push('\n');
    }

    // Canonicalized resource: /account/path (no query string here since we use
    // no query params for the Shared Key GET).
    let canon_resource = format!("/{account}{path}");

    let string_to_sign = format!(
        "GET\n\
         \n\
         \n\
         \n\
         \n\
         \n\
         \n\
         \n\
         \n\
         \n\
         \n\
         \n\
         {canon_headers}{canon_resource}"
    );

    let mut mac = HmacSha256::new_from_slice(&key)
        .map_err(|e| format!("azfs: bad HMAC key length: {e}"))?;
    mac.update(string_to_sign.as_bytes());
    let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    let authorization = format!("SharedKey {account}:{sig}");
    let url = format!("https://{host}{path}");
    Ok(SignedRequest {
        url,
        headers: vec![
            ("x-ms-date".to_string(), x_ms_date.to_string()),
            ("x-ms-version".to_string(), x_ms_version.to_string()),
            ("Authorization".to_string(), authorization),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let p = parse_az_url("az://mycontainer/path/to/data.parquet").unwrap();
        assert_eq!(p.container, "mycontainer");
        assert_eq!(p.blob, "path/to/data.parquet");
    }

    #[test]
    fn parse_azure_scheme_and_trim() {
        let p = parse_az_url("azure://c/b.csv").unwrap();
        assert_eq!(p.container, "c");
        assert_eq!(p.blob, "b.csv");
    }

    #[test]
    fn parse_rejects_bad() {
        assert!(parse_az_url("http://x/y").is_err());
        assert!(parse_az_url("az://onlycontainer").is_err());
        assert!(parse_az_url("az:///blob").is_err());
    }

    #[test]
    fn url_to_https_blob_mapping() {
        let creds = Credentials {
            account: Some("myacct".into()),
            ..Default::default()
        };
        let p = parse_az_url("az://data/2026/file.parquet").unwrap();
        let (host, path) = blob_host_and_path(&creds, &p).unwrap();
        assert_eq!(host, "myacct.blob.core.windows.net");
        assert_eq!(path, "/data/2026/file.parquet");
    }

    #[test]
    fn sas_url_append() {
        let url = build_sas_url(
            "myacct.blob.core.windows.net",
            "/data/file.parquet",
            "?sv=2022-11-02&ss=b&srt=co&sp=r&sig=abc%2Bdef",
        );
        assert_eq!(
            url,
            "https://myacct.blob.core.windows.net/data/file.parquet?sv=2022-11-02&ss=b&srt=co&sp=r&sig=abc%2Bdef"
        );
        // Also works when the SAS lacks a leading '?'.
        let url2 = build_sas_url("h", "/c/b", "sv=1&sig=x");
        assert_eq!(url2, "https://h/c/b?sv=1&sig=x");
    }

    #[test]
    fn connection_string_parse() {
        let cs = "DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=a2V5MTIz==;EndpointSuffix=core.windows.net";
        let c = parse_connection_string(cs);
        assert_eq!(c.account.as_deref(), Some("acct"));
        assert_eq!(c.account_key.as_deref(), Some("a2V5MTIz=="));
        assert_eq!(c.endpoint_suffix.as_deref(), Some("core.windows.net"));
    }

    #[test]
    fn connection_string_with_sas() {
        let cs = "BlobEndpoint=https://acct.blob.core.windows.net;SharedAccessSignature=sv=2022-11-02&sig=zzz";
        let c = parse_connection_string(cs);
        assert_eq!(c.sas_token.as_deref(), Some("sv=2022-11-02&sig=zzz"));
    }

    #[test]
    fn resolve_precedence() {
        // Connection string wins for account; env fills SAS.
        let env = |k: &str| -> Option<String> {
            match k {
                "AZURE_STORAGE_CONNECTION_STRING" => {
                    Some("AccountName=fromcs;AccountKey=a2V5==".into())
                }
                "AZURE_STORAGE_SAS_TOKEN" => Some("sv=1&sig=x".into()),
                _ => None,
            }
        };
        let c = resolve_credentials(env);
        assert_eq!(c.account.as_deref(), Some("fromcs"));
        assert_eq!(c.account_key.as_deref(), Some("a2V5=="));
        assert_eq!(c.sas_token.as_deref(), Some("sv=1&sig=x"));
    }

    /// Known-vector test for Shared Key HMAC-SHA256.
    ///
    /// We verify our HMAC-SHA256 pipeline against an independently computable
    /// vector: key = base64("0123456789") signing the exact string-to-sign our
    /// signer produces. The expected signature below was computed with a
    /// reference HMAC-SHA256 over that string-to-sign and base64-encoded.
    #[test]
    fn shared_key_signing_known_vector() {
        // base64 of the raw key bytes "0123456789".
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(b"0123456789");
        let signed = sign_shared_key(
            "myaccount",
            &key_b64,
            "myaccount.blob.core.windows.net",
            "/mycontainer/myblob.txt",
            "Mon, 27 Jul 2009 12:28:53 GMT",
            "2021-08-06",
        )
        .unwrap();

        // The Authorization header is "SharedKey <account>:<sig>".
        let auth = &signed
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .unwrap()
            .1;
        assert!(auth.starts_with("SharedKey myaccount:"));

        // Recompute the expected signature independently to lock the canonical
        // string-to-sign layout (regression guard against header ordering /
        // empty-line drift).
        let canon = "\
GET\n\n\n\n\n\n\n\n\n\n\n\n\
x-ms-date:Mon, 27 Jul 2009 12:28:53 GMT\n\
x-ms-version:2021-08-06\n\
/myaccount/mycontainer/myblob.txt";
        let mut mac = HmacSha256::new_from_slice(b"0123456789").unwrap();
        mac.update(canon.as_bytes());
        let expected =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert_eq!(*auth, format!("SharedKey myaccount:{expected}"));

        // And the URL must be the plain https blob URL (no query for SharedKey).
        assert_eq!(
            signed.url,
            "https://myaccount.blob.core.windows.net/mycontainer/myblob.txt"
        );
    }
}
