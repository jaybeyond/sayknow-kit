//! AWS Signature Version 4 request signing (header-based) for the R2 adapter.
//!
//! The canonical request follows the AWS SigV4 spec with the S3 rules: the
//! path is URI-encoded once with `/` preserved, the query is sorted by key and
//! then value with RFC 3986 encoding, header names are lowercased and values
//! trimmed with inner whitespace runs collapsed. Every header present on the
//! request is signed, so what goes on the wire is what was signed.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use sha2::{Digest, Sha256};

use super::transport::HttpRequest;

type HmacSha256 = Hmac<Sha256>;

/// RFC 3986 unreserved characters stay literal; everything else is encoded.
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
/// Same as [`UNRESERVED`] but keeps the path separator (S3 canonical URI).
const PATH_SAFE: &AsciiSet = &UNRESERVED.remove(b'/');

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// An access key pair. `Debug` never prints the secret.
#[derive(Clone)]
pub(crate) struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

/// Lowercase hex SHA-256 of `data`.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Signs `req` in place: adds `x-amz-date`, `x-amz-content-sha256` and
/// `Authorization`. Fails only when the request URL cannot be parsed.
pub(crate) fn sign(
    req: &mut HttpRequest,
    creds: &Credentials,
    region: &str,
    service: &str,
    now: SystemTime,
    payload_sha256_hex: &str,
) -> Result<(), String> {
    sign_inner(req, creds, region, service, now, payload_sha256_hex, true)
}

/// `with_content_sha_header == false` exists only so the AWS test-suite
/// vectors that do not carry `x-amz-content-sha256` can be checked.
fn sign_inner(
    req: &mut HttpRequest,
    creds: &Credentials,
    region: &str,
    service: &str,
    now: SystemTime,
    payload_sha256_hex: &str,
    with_content_sha_header: bool,
) -> Result<(), String> {
    let url = url::Url::parse(&req.url).map_err(|_| "invalid request URL".to_owned())?;
    let host = match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_owned(),
        (None, _) => return Err("request URL has no host".to_owned()),
    };
    let amz_date = amz_date(now);
    let date = &amz_date[..8];

    req.headers.retain(|(name, _)| {
        let n = name.to_ascii_lowercase();
        n != "authorization" && n != "x-amz-date" && n != "x-amz-content-sha256"
    });
    if !req.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("host")) {
        req.headers.push(("host".to_owned(), host));
    }
    req.headers.push(("x-amz-date".to_owned(), amz_date.clone()));
    if with_content_sha_header {
        req.headers.push(("x-amz-content-sha256".to_owned(), payload_sha256_hex.to_owned()));
    }

    let canonical_uri = canonical_uri(url.path());
    let canonical_query = canonical_query(url.query().unwrap_or(""));
    let (canonical_headers, signed_headers) = canonical_headers(&req.headers);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method, canonical_uri, canonical_query, canonical_headers, signed_headers, payload_sha256_hex
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let k_date = hmac(format!("AWS4{}", creds.secret_key).as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));

    req.headers.push((
        "Authorization".to_owned(),
        format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.access_key
        ),
    ));
    Ok(())
}

/// The request path as it appears in the URL, decoded and re-encoded once so
/// an already-encoded path is not double-encoded.
fn canonical_uri(path: &str) -> String {
    let decoded = percent_decode_str(path).collect::<Vec<u8>>();
    let decoded = String::from_utf8_lossy(&decoded);
    let encoded = utf8_percent_encode(&decoded, PATH_SAFE).to_string();
    if encoded.is_empty() {
        "/".to_owned()
    } else {
        encoded
    }
}

fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (encode_component(k), encode_component(v))
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Decodes a raw query component (a literal `+` stays a `+`) and re-encodes
/// it per RFC 3986.
fn encode_component(raw: &str) -> String {
    let decoded = percent_decode_str(raw).collect::<Vec<u8>>();
    utf8_percent_encode(&String::from_utf8_lossy(&decoded), UNRESERVED).to_string()
}

/// Returns (canonical header block, signed header list).
fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut entries: Vec<(String, String)> = Vec::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        match entries.iter_mut().find(|(n, _)| *n == name) {
            Some((_, existing)) => {
                existing.push(',');
                existing.push_str(&value);
            }
            None => entries.push((name, value)),
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let block = entries.iter().map(|(n, v)| format!("{n}:{v}\n")).collect::<String>();
    let signed = entries.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(";");
    (block, signed)
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

/// `YYYYMMDD'T'HHMMSS'Z'` in UTC. Times before the epoch clamp to the epoch.
fn amz_date(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, (rem / 60) % 60, rem % 60)
}

/// Howard Hinnant's days-to-civil conversion (proleptic Gregorian, UTC).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 2015-08-30T12:36:00Z, the timestamp of every aws-signing-test-suite case.
    fn suite_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_440_938_160)
    }

    fn suite_creds() -> Credentials {
        Credentials {
            access_key: "AKIDEXAMPLE".to_owned(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
        }
    }

    fn req(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> HttpRequest {
        HttpRequest {
            method: method.to_owned(),
            url: url.to_owned(),
            headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
            body: body.to_vec(),
        }
    }

    fn authorization(req: &HttpRequest) -> &str {
        &req.headers.iter().find(|(n, _)| n == "Authorization").expect("signed").1
    }

    /// Signs one aws-signing-test-suite case. Those cases sign without the
    /// `x-amz-content-sha256` header unless `sign_body` is true.
    fn suite_auth(mut r: HttpRequest, sign_body: bool) -> String {
        let payload = sha256_hex(&r.body);
        sign_inner(&mut r, &suite_creds(), "us-east-1", "service", suite_time(), &payload, sign_body).unwrap();
        authorization(&r).to_owned()
    }

    #[test]
    fn amz_date_formats_utc() {
        assert_eq!(amz_date(suite_time()), "20150830T123600Z");
        assert_eq!(amz_date(UNIX_EPOCH), "19700101T000000Z");
        // 2000-02-29 (leap day) 23:59:59.
        assert_eq!(amz_date(UNIX_EPOCH + Duration::from_secs(951_868_799)), "20000229T235959Z");
    }

    // Official AWS vector. Source (awslabs/aws-c-auth aws-signing-test-suite):
    // https://raw.githubusercontent.com/awslabs/aws-c-auth/main/tests/aws-signing-test-suite/v4/get-vanilla/header-signed-request.txt
    // https://raw.githubusercontent.com/awslabs/aws-c-auth/main/tests/aws-signing-test-suite/v4/get-vanilla/context.json
    #[test]
    fn aws_vector_get_vanilla() {
        let auth = suite_auth(req("GET", "https://example.amazonaws.com/", &[], b""), false);
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    // Official AWS vector (query sorting). Source:
    // https://raw.githubusercontent.com/awslabs/aws-c-auth/main/tests/aws-signing-test-suite/v4/get-vanilla-query-order-key-case/header-signed-request.txt
    #[test]
    fn aws_vector_get_vanilla_query_order_key_case() {
        let auth = suite_auth(
            req("GET", "https://example.amazonaws.com/?Param2=value2&Param1=value1", &[], b""),
            false,
        );
        assert!(auth.ends_with(
            "SignedHeaders=host;x-amz-date, \
             Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
        ));
    }

    // Official AWS vector (header trimming / whitespace collapsing). Source:
    // https://raw.githubusercontent.com/awslabs/aws-c-auth/main/tests/aws-signing-test-suite/v4/get-header-value-trim/header-signed-request.txt
    // https://raw.githubusercontent.com/awslabs/aws-c-auth/main/tests/aws-signing-test-suite/v4/get-header-value-trim/header-canonical-request.txt
    #[test]
    fn aws_vector_get_header_value_trim() {
        let auth = suite_auth(
            req(
                "GET",
                "https://example.amazonaws.com/",
                &[("My-Header1", " value1"), ("My-Header2", " \"a   b   c\"")],
                b"",
            ),
            false,
        );
        assert!(auth.ends_with(
            "SignedHeaders=host;my-header1;my-header2;x-amz-date, \
             Signature=acc3ed3afb60bb290fc8d2dd0098b9911fcaa05412b367055dee359757a9c736"
        ));
    }

    // Official AWS vector (unreserved characters in the path). Source:
    // https://raw.githubusercontent.com/awslabs/aws-c-auth/main/tests/aws-signing-test-suite/v4/get-unreserved/header-signed-request.txt
    #[test]
    fn aws_vector_get_unreserved() {
        let auth = suite_auth(
            req(
                "GET",
                "https://example.amazonaws.com/-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
                &[],
                b"",
            ),
            false,
        );
        assert!(auth.ends_with("Signature=07ef7494c76fa4850883e2b006601f940f8a34d404d0cfa977f52a65bbf5f24f"));
    }

    // Official AWS vector, signed body through the public `sign` (it carries
    // x-amz-content-sha256 exactly as `sign` does). Sources:
    // https://raw.githubusercontent.com/awslabs/aws-c-auth/main/tests/aws-signing-test-suite/v4/post-x-www-form-urlencoded/header-signed-request.txt
    // https://raw.githubusercontent.com/awslabs/aws-c-auth/main/tests/aws-signing-test-suite/v4/post-x-www-form-urlencoded/context.json
    #[test]
    fn aws_vector_post_x_www_form_urlencoded_via_public_sign() {
        let body = b"Param1=value1";
        let mut r = req(
            "POST",
            "https://example.amazonaws.com/",
            &[("Content-Type", "application/x-www-form-urlencoded"), ("Content-Length", "13")],
            body,
        );
        let payload = sha256_hex(body);
        assert_eq!(payload, "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e");
        sign(&mut r, &suite_creds(), "us-east-1", "service", suite_time(), &payload).unwrap();
        assert_eq!(
            authorization(&r),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=content-length;content-type;host;x-amz-content-sha256;x-amz-date, \
             Signature=d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b"
        );
        let header = |name: &str| r.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
        assert_eq!(header("x-amz-date"), Some("20150830T123600Z"));
        assert_eq!(header("x-amz-content-sha256"), Some(payload.as_str()));
    }

    // R2 vector (region "auto", service "s3", path-style). The expected
    // signature was computed by the independent pure-Python hmac/hashlib signer
    // /Volumes/jmin-G/Dev/sayknow_kit/_sync/g5-r2-vector.py, whose recorded
    // output (2026-10-10) is:
    //   signature=329893e4cb77c4f304f0e99991609d531f82fe18c1cb5a3dc893cd005f4712f1
    #[test]
    fn r2_vector_matches_independent_python_signer() {
        let body = b"segment-bytes";
        let mut r = req(
            "PUT",
            "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/sayknow-vault/sayknow/v1/vault-a/log/dev-a/1-3.seg",
            &[("If-None-Match", "*")],
            body,
        );
        let creds = Credentials {
            access_key: "R2TESTACCESSKEYID0000".to_owned(),
            secret_key: "r2-test-secret-access-key-0000000000000000000000000000000000000000".to_owned(),
        };
        // 2026-10-10T12:00:00Z
        let now = UNIX_EPOCH + Duration::from_secs(1_791_633_600);
        assert_eq!(amz_date(now), "20261010T120000Z");
        sign(&mut r, &creds, "auto", "s3", now, &sha256_hex(body)).unwrap();
        assert_eq!(
            authorization(&r),
            "AWS4-HMAC-SHA256 Credential=R2TESTACCESSKEYID0000/20261010/auto/s3/aws4_request, \
             SignedHeaders=host;if-none-match;x-amz-content-sha256;x-amz-date, \
             Signature=329893e4cb77c4f304f0e99991609d531f82fe18c1cb5a3dc893cd005f4712f1"
        );
    }

    #[test]
    fn resigning_replaces_previous_signature_headers() {
        let mut r = req("GET", "https://example.amazonaws.com/", &[], b"");
        let payload = sha256_hex(b"");
        sign(&mut r, &suite_creds(), "us-east-1", "service", suite_time(), &payload).unwrap();
        sign(&mut r, &suite_creds(), "us-east-1", "service", suite_time(), &payload).unwrap();
        for name in ["Authorization", "x-amz-date", "x-amz-content-sha256", "host"] {
            assert_eq!(r.headers.iter().filter(|(n, _)| n == name).count(), 1, "{name}");
        }
    }

    #[test]
    fn credentials_debug_redacts_secret() {
        let text = format!("{:?}", suite_creds());
        assert!(!text.contains("wJalrXUtnFEMI"));
        assert!(text.contains("<redacted>"));
    }
}
