//! Low-privilege subscription download. A result is not an accepted source until
//! the caller has completely parsed it and durably committed its content binding.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use url::Url;

pub use crate::https_reader::ReaderError as DownloadError;
use crate::https_reader::{HttpsReader, WireResponse, global_address, validate_headers};

pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const REPRESENTATION: &str = "text/plain;accept-encoding=gzip, identity;v=1";
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
const BUFFER: usize = 16 * 1024;
static DOWNLOAD: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Validators {
    pub final_url: String,
    pub representation: String,
    pub content_sha256: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}
impl Validators {
    pub fn validate(&self) -> Result<(), DownloadError> {
        if canonical_url(&self.final_url)? != self.final_url
            || self.representation != REPRESENTATION
            || !valid_hash(&self.content_sha256)
            || self.etag.as_ref().is_some_and(|v| !valid_etag(v))
            || self
                .last_modified
                .as_ref()
                .is_some_and(|v| !valid_modified(v))
        {
            return Err(DownloadError::new("invalid_validators"));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Downloaded {
    pub sha256: String,
    pub bytes: u64,
    pub transferred_bytes: u64,
    pub final_url: String,
    pub validators: Validators,
}
#[derive(Debug)]
pub enum DownloadOutcome {
    Downloaded(Downloaded),
    NotModified,
}

#[derive(Clone)]
pub struct SubscriptionReader {
    transport: HttpsReader,
}
impl Default for SubscriptionReader {
    fn default() -> Self {
        Self::new()
    }
}
impl SubscriptionReader {
    pub fn new() -> Self {
        Self {
            transport: HttpsReader::new(),
        }
    }

    /// The caller supplies a safe empty staging descriptor and owns disk quota,
    /// cleanup, complete parsing and commit. `verified_lkg_sha256` may only name
    /// an object whose complete bytes have been verified by the source store.
    pub async fn download<W: AsyncWrite + Unpin>(
        &self,
        url: &str,
        previous: Option<&Validators>,
        verified_lkg_sha256: Option<&str>,
        output: &mut W,
    ) -> Result<DownloadOutcome, DownloadError> {
        let _permit = DOWNLOAD
            .try_acquire()
            .map_err(|_| DownloadError::new("subscription_busy"))?;
        tokio::time::timeout(
            TOTAL_TIMEOUT,
            download_with(
                url,
                previous,
                verified_lkg_sha256,
                output,
                |uri, headers| self.transport.request(uri, headers),
                MAX_BYTES,
            ),
        )
        .await
        .map_err(|_| DownloadError::new("request_timeout"))?
    }
}

pub fn canonical_url(value: &str) -> Result<String, DownloadError> {
    if !value
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        || value.contains('\\')
        || value.len() > 2048
        || value
            .chars()
            .any(|c| c.is_ascii_control() || c.is_ascii_whitespace())
        || authority_has_userinfo(value)
    {
        return Err(DownloadError::new("destination_not_allowed"));
    }
    let url = Url::parse(value).map_err(|_| DownloadError::new("destination_not_allowed"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
        || url.port() == Some(0)
        || url.as_str().len() > 2048
    {
        return Err(DownloadError::new("destination_not_allowed"));
    }
    let ip = match url.host().expect("checked host") {
        url::Host::Ipv4(ip) => Some(ip.into()),
        url::Host::Ipv6(ip) => Some(ip.into()),
        url::Host::Domain(_) => None,
    };
    if ip.is_some_and(|ip| !global_address(ip)) {
        return Err(DownloadError::new("destination_not_allowed"));
    }
    Ok(url.into())
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn authority_has_userinfo(value: &str) -> bool {
    value
        .split_once("://")
        .map(|(_, rest)| rest)
        .or_else(|| value.strip_prefix("//"))
        .is_some_and(|rest| {
            rest.split(['/', '\\', '?', '#'])
                .next()
                .is_some_and(|authority| authority.contains('@'))
        })
}
fn valid_header(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && HeaderValue::from_str(value).is_ok()
}
fn valid_etag(value: &str) -> bool {
    let opaque = value.strip_prefix("W/").unwrap_or(value);
    valid_header(value)
        && opaque.starts_with('"')
        && opaque.ends_with('"')
        && opaque.len() >= 2
        && opaque.as_bytes()[1..opaque.len() - 1]
            .iter()
            .all(|b| *b == 0x21 || (0x23..=0x7e).contains(b) || *b >= 0x80)
}
fn valid_modified(value: &str) -> bool {
    valid_header(value) && httpdate::parse_http_date(value).is_ok()
}
fn saved_header(headers: &HeaderMap, name: http::HeaderName) -> Option<String> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    (values.next().is_none() && valid_header(value)).then(|| value.to_owned())
}
fn request_headers(binding: Option<&Validators>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("PariNS-subscriptions/1"),
    );
    headers.insert(header::ACCEPT, HeaderValue::from_static("text/plain"));
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, identity"),
    );
    if let Some(binding) = binding {
        if let Some(etag) = &binding.etag {
            headers.insert(
                header::IF_NONE_MATCH,
                HeaderValue::from_str(etag).expect("validated binding"),
            );
        } else if let Some(modified) = &binding.last_modified {
            headers.insert(
                header::IF_MODIFIED_SINCE,
                HeaderValue::from_str(modified).expect("validated binding"),
            );
        }
    }
    headers
}

async fn download_with<W, F, Fut>(
    url: &str,
    previous: Option<&Validators>,
    verified_lkg_sha256: Option<&str>,
    output: &mut W,
    mut request: F,
    limit: u64,
) -> Result<DownloadOutcome, DownloadError>
where
    W: AsyncWrite + Unpin,
    F: FnMut(Uri, HeaderMap) -> Fut,
    Fut: Future<Output = Result<WireResponse, DownloadError>>,
{
    if let Some(previous) = previous {
        previous.validate()?;
    }
    let mut current = canonical_url(url)?;
    let mut redirects = 0;
    let mut repaired = false;
    loop {
        let binding = previous.filter(|v| {
            !repaired
                && v.final_url == current
                && verified_lkg_sha256 == Some(v.content_sha256.as_str())
        });
        let headers = request_headers(binding);
        let conditional = headers.contains_key(header::IF_NONE_MATCH)
            || headers.contains_key(header::IF_MODIFIED_SINCE);
        let response = request(
            current
                .parse()
                .map_err(|_| DownloadError::new("destination_not_allowed"))?,
            headers,
        )
        .await?;
        validate_headers(&response.headers)?;
        if matches!(response.status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            if redirects == 3 {
                return Err(DownloadError::new("redirect_limit"));
            }
            let location = response
                .headers
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .filter(|v| {
                    v.len() <= 2048
                        && !v.contains('\\')
                        && !v
                            .chars()
                            .any(|c| c.is_ascii_control() || c.is_ascii_whitespace())
                        && !authority_has_userinfo(v)
                })
                .ok_or_else(|| DownloadError::new("destination_not_allowed"))?;
            let joined = Url::parse(&current)
                .expect("canonical URL")
                .join(location)
                .map_err(|_| DownloadError::new("destination_not_allowed"))?;
            current = canonical_url(joined.as_str())?;
            redirects += 1;
            continue;
        }
        if response.status == StatusCode::NOT_MODIFIED {
            if conditional {
                return Ok(DownloadOutcome::NotModified);
            }
            if repaired {
                return Err(DownloadError::new("invalid_not_modified"));
            }
            repaired = true;
            continue;
        }
        if response.status != StatusCode::OK {
            return Err(http_error(response.status, &response.headers));
        }
        let etag = saved_header(&response.headers, header::ETAG).filter(|v| valid_etag(v));
        let last_modified =
            saved_header(&response.headers, header::LAST_MODIFIED).filter(|v| valid_modified(v));
        let (sha256, bytes, transferred_bytes) = stream_text(response, output, limit).await?;
        let validators = Validators {
            final_url: current.clone(),
            representation: REPRESENTATION.into(),
            content_sha256: sha256.clone(),
            etag,
            last_modified,
        };
        return Ok(DownloadOutcome::Downloaded(Downloaded {
            sha256,
            bytes,
            transferred_bytes,
            final_url: current,
            validators,
        }));
    }
}

async fn stream_text<W: AsyncWrite + Unpin>(
    mut response: WireResponse,
    output: &mut W,
    limit: u64,
) -> Result<(String, u64, u64), DownloadError> {
    let mut encodings = response.headers.get_all(header::CONTENT_ENCODING).iter();
    let encoding = encodings
        .next()
        .map(|v| v.to_str().unwrap_or("invalid"))
        .unwrap_or("identity");
    if encodings.next().is_some()
        || !(encoding.eq_ignore_ascii_case("identity") || encoding.eq_ignore_ascii_case("gzip"))
    {
        return Err(DownloadError::new("unsupported_encoding"));
    }
    let length = response
        .headers
        .get(header::CONTENT_LENGTH)
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| DownloadError::new("invalid_response"))
        })
        .transpose()?;
    if length.is_some_and(|n| n > limit) {
        return Err(DownloadError::new("transfer_too_large"));
    }
    // zlib validates the complete gzip framing, including optional header CRC and
    // trailer CRC/length. Only fixed-size output is exposed before the next await.
    let mut decoder = encoding
        .eq_ignore_ascii_case("gzip")
        .then(|| flate2::Decompress::new_gzip(15));
    let mut member_finished = false;
    let mut transferred = 0u64;
    let mut written = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; BUFFER];
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk?;
        transferred = transferred
            .checked_add(chunk.len() as u64)
            .filter(|n| *n <= limit)
            .ok_or_else(|| DownloadError::new("transfer_too_large"))?;
        if chunk.is_empty() {
            continue;
        }
        if let Some(decoder) = decoder.as_mut() {
            let mut offset = 0;
            loop {
                if member_finished {
                    // RFC gzip members may concatenate, but any trailing bytes
                    // must form another complete, validated member.
                    *decoder = flate2::Decompress::new_gzip(15);
                    member_finished = false;
                }
                let before_in = decoder.total_in();
                let before_out = decoder.total_out();
                let status = decoder
                    .decompress(&chunk[offset..], &mut buffer, flate2::FlushDecompress::None)
                    .map_err(|_| DownloadError::new("invalid_response"))?;
                let consumed = (decoder.total_in() - before_in) as usize;
                let produced = (decoder.total_out() - before_out) as usize;
                offset += consumed;
                write_decoded(output, &buffer[..produced], &mut written, &mut hash, limit).await?;
                if status == flate2::Status::StreamEnd {
                    member_finished = true;
                    if offset == chunk.len() {
                        break;
                    }
                } else if consumed == 0 && produced == 0 {
                    if offset != chunk.len() {
                        return Err(DownloadError::new("invalid_response"));
                    }
                    break;
                } else if offset == chunk.len() && produced < buffer.len() {
                    break;
                }
            }
        } else {
            for part in chunk.chunks(BUFFER) {
                write_decoded(output, part, &mut written, &mut hash, limit).await?;
            }
        }
    }
    if (decoder.is_some() && !member_finished) || length.is_some_and(|n| n != transferred) {
        return Err(DownloadError::new("invalid_response"));
    }
    output
        .flush()
        .await
        .map_err(|_| DownloadError::new("file_write_failed"))?;
    Ok((format!("{:x}", hash.finalize()), written, transferred))
}

async fn write_decoded<W: AsyncWrite + Unpin>(
    output: &mut W,
    chunk: &[u8],
    written: &mut u64,
    hash: &mut Sha256,
    limit: u64,
) -> Result<(), DownloadError> {
    *written = written
        .checked_add(chunk.len() as u64)
        .filter(|n| *n <= limit)
        .ok_or_else(|| DownloadError::new("decoded_too_large"))?;
    output
        .write_all(chunk)
        .await
        .map_err(|_| DownloadError::new("file_write_failed"))?;
    hash.update(chunk);
    Ok(())
}

fn http_error(status: StatusCode, headers: &HeaderMap) -> DownloadError {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let retry_after_unix = if matches!(status.as_u16(), 429 | 503) {
        headers
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.parse::<u64>()
                    .ok()
                    .map(|seconds| now.saturating_add(seconds))
                    .or_else(|| {
                        httpdate::parse_http_date(v)
                            .ok()?
                            .duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs())
                    })
            })
    } else {
        None
    };
    DownloadError {
        code: if status == StatusCode::TOO_MANY_REQUESTS {
            "rate_limited"
        } else {
            "http_error"
        },
        retry_after_unix,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) async fn download_fixture<W: AsyncWrite + Unpin>(
    url: &str,
    previous: Option<&Validators>,
    verified_lkg_sha256: Option<&str>,
    output: &mut W,
    responses: Vec<WireResponse>,
) -> Result<DownloadOutcome, DownloadError> {
    let mut responses = std::collections::VecDeque::from(responses);
    download_with(
        url,
        previous,
        verified_lkg_sha256,
        output,
        |_, _| {
            std::future::ready(
                responses
                    .pop_front()
                    .ok_or_else(|| DownloadError::new("fixture_exhausted")),
            )
        },
        MAX_BYTES,
    )
    .await
}
