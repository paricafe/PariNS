//! Bounded, unauthenticated official-release transport shared by app and helper.
//! No caller-supplied URL, proxy, TLS roots, or command is accepted.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use crate::https_reader::ReaderError;
#[cfg(test)]
use crate::https_reader::global_address;
use crate::https_reader::{HttpsReader, WireResponse};
#[cfg(test)]
use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::Semaphore, time::timeout};

use super::contract::{
    ContractError, GithubRelease, MAX_BINARY_BYTES, Manifest, Version, valid_sha256,
};

const METADATA_LIMIT: usize = 512 * 1024;
const MANIFEST_LIMIT: usize = 64 * 1024;
const METADATA_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
static DOWNLOAD: Semaphore = Semaphore::const_new(1);

impl From<ContractError> for ReaderError {
    fn from(value: ContractError) -> Self {
        Self::new(match value {
            ContractError::InvalidVersion => "invalid_version",
            ContractError::InvalidManifest => "invalid_manifest",
            ContractError::ReleaseIncomplete => "release_incomplete",
            ContractError::ReleaseChanged => "release_changed",
            ContractError::ManualRequired => "manual_upgrade_required",
            ContractError::NotNewer => "not_newer",
        })
    }
}

#[derive(Debug)]
pub enum Latest {
    Modified {
        release: GithubRelease,
        etag: Option<String>,
    },
    /// The coordinator must already possess a validated cached result.
    NotModified,
}

#[derive(Clone, Debug)]
pub struct ManifestDocument {
    pub manifest: Manifest,
    pub sha256: String,
    pub size: u64,
}
impl ManifestDocument {
    pub fn validate_release(&self, release: &GithubRelease) -> Result<(), ReaderError> {
        self.manifest.validate_release(release)?;
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == "parins-update.json")
            .ok_or_else(|| ReaderError::new("release_incomplete"))?;
        if asset.size != self.size
            || asset
                .digest
                .as_ref()
                .is_some_and(|d| d != &format!("sha256:{}", self.sha256))
        {
            return Err(ReaderError::new("release_changed"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DownloadTuple {
    pub release_id: u64,
    pub tag: String,
    pub manifest_sha256: String,
    pub asset_id: u64,
    pub asset_name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone)]
pub struct GithubReader {
    transport: HttpsReader,
}
impl Default for GithubReader {
    fn default() -> Self {
        Self::new()
    }
}
impl GithubReader {
    pub fn new() -> Self {
        Self {
            transport: HttpsReader::new(),
        }
    }

    pub async fn latest(&self, etag: Option<&str>) -> Result<Latest, ReaderError> {
        timeout(METADATA_TIMEOUT, async {
            let response = self
                .fetch(
                    fixed_uri("https://api.github.com/repos/paricafe/PariNS/releases/latest")?,
                    etag,
                )
                .await?;
            if response.status == StatusCode::NOT_MODIFIED {
                return Ok(Latest::NotModified);
            }
            let etag = response
                .headers
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.len() <= 1024)
                .map(str::to_owned);
            let release = parse_release(&collect(response, METADATA_LIMIT).await?)?;
            Ok(Latest::Modified { release, etag })
        })
        .await
        .map_err(|_| ReaderError::new("request_timeout"))?
    }

    pub async fn release(&self, id: u64) -> Result<GithubRelease, ReaderError> {
        if id == 0 {
            return Err(ReaderError::new("release_changed"));
        }
        timeout(METADATA_TIMEOUT, async {
            let uri = fixed_uri(&format!(
                "https://api.github.com/repos/paricafe/PariNS/releases/{id}"
            ))?;
            let release =
                parse_release(&collect(self.fetch(uri, None).await?, METADATA_LIMIT).await?)?;
            if release.id != id {
                return Err(ReaderError::new("release_changed"));
            }
            Ok(release)
        })
        .await
        .map_err(|_| ReaderError::new("request_timeout"))?
    }

    pub async fn manifest(&self, tag: &str) -> Result<ManifestDocument, ReaderError> {
        Version::parse_tag(tag)?;
        timeout(METADATA_TIMEOUT, async {
            let bytes = collect(
                self.fetch(asset_uri(tag, "parins-update.json")?, None)
                    .await?,
                MANIFEST_LIMIT,
            )
            .await?;
            let manifest: Manifest =
                serde_json::from_slice(&bytes).map_err(|_| ReaderError::new("invalid_manifest"))?;
            manifest.validate()?;
            if manifest.tag != tag {
                return Err(ReaderError::new("release_changed"));
            }
            Ok(ManifestDocument {
                manifest,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                size: bytes.len() as u64,
            })
        })
        .await
        .map_err(|_| ReaderError::new("request_timeout"))?
    }

    /// Independently revalidate the confirmed release before opening its stream.
    /// Caller owns the safe empty file descriptor, fsync, ELF checks, and cleanup.
    pub async fn download(
        &self,
        tuple: &DownloadTuple,
        file: &mut tokio::fs::File,
        mut progress: impl FnMut(u64),
    ) -> Result<(), ReaderError> {
        let _permit = DOWNLOAD
            .try_acquire()
            .map_err(|_| ReaderError::new("update_in_progress"))?;
        timeout(DOWNLOAD_TIMEOUT, async {
            Version::parse_tag(&tuple.tag)?;
            if !valid_sha256(&tuple.sha256)
                || !valid_sha256(&tuple.manifest_sha256)
                || tuple.size > MAX_BINARY_BYTES
            {
                return Err(ReaderError::new("verification_failed"));
            }
            let release = self.release(tuple.release_id).await?;
            let document = self.manifest(&tuple.tag).await?;
            document.validate_release(&release)?;
            verify_tuple(tuple, &release, &document)?;
            let response = self
                .fetch(asset_uri(&tuple.tag, &tuple.asset_name)?, None)
                .await?;
            stream_binary(response, file, tuple, &mut progress).await
        })
        .await
        .map_err(|_| ReaderError::new("request_timeout"))?
    }

    async fn fetch(&self, uri: Uri, etag: Option<&str>) -> Result<WireResponse, ReaderError> {
        fetch_with(uri, etag, |uri, etag| self.request(uri, etag)).await
    }

    async fn request(&self, uri: Uri, etag: Option<String>) -> Result<WireResponse, ReaderError> {
        let host = validate_uri(&uri)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("PariNS-updater/1"),
        );
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        if host == "api.github.com" {
            headers.insert(
                header::ACCEPT,
                HeaderValue::from_static("application/vnd.github+json"),
            );
            headers.insert(
                "x-github-api-version",
                HeaderValue::from_static("2026-03-10"),
            );
            if let Some(etag) = etag {
                headers.insert(
                    header::IF_NONE_MATCH,
                    HeaderValue::from_str(&etag).map_err(|_| ReaderError::new("invalid_etag"))?,
                );
            }
        } else {
            headers.insert(
                header::ACCEPT,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
        self.transport.request(uri, headers).await
    }
}

fn verify_tuple(
    tuple: &DownloadTuple,
    release: &GithubRelease,
    document: &ManifestDocument,
) -> Result<(), ReaderError> {
    let artifact = document
        .manifest
        .artifacts
        .iter()
        .find(|a| a.name == tuple.asset_name)
        .ok_or_else(|| ReaderError::new("release_changed"))?;
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == tuple.asset_name)
        .ok_or_else(|| ReaderError::new("release_changed"))?;
    if document.sha256 != tuple.manifest_sha256
        || release.id != tuple.release_id
        || release.tag_name != tuple.tag
        || asset.id != tuple.asset_id
        || artifact.size != tuple.size
        || artifact.sha256 != tuple.sha256
    {
        return Err(ReaderError::new("release_changed"));
    }
    Ok(())
}

async fn fetch_with<F, Fut>(
    mut uri: Uri,
    etag: Option<&str>,
    mut request: F,
) -> Result<WireResponse, ReaderError>
where
    F: FnMut(Uri, Option<String>) -> Fut,
    Fut: Future<Output = Result<WireResponse, ReaderError>>,
{
    let etag = etag
        .map(|value| {
            if value.len() > 1024 || HeaderValue::from_str(value).is_err() {
                return Err(ReaderError::new("invalid_etag"));
            }
            Ok(value.to_owned())
        })
        .transpose()?;
    for redirects in 0..=3 {
        validate_uri(&uri)?;
        let response = request(uri.clone(), etag.clone()).await?;
        if matches!(response.status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            if redirects == 3 {
                return Err(ReaderError::new("redirect_limit"));
            }
            let location = response
                .headers
                .get(header::LOCATION)
                .and_then(|h| h.to_str().ok())
                .ok_or_else(|| ReaderError::new("destination_not_allowed"))?;
            uri = redirect_uri(&uri, location)?;
            continue;
        }
        if response.status == StatusCode::NOT_MODIFIED && etag.is_some() {
            return Ok(response);
        }
        if response.status != StatusCode::OK {
            return Err(http_error(response.status, &response.headers, unix_now()));
        }
        // No decompressor is enabled: a server ignoring identity is rejected.
        if response
            .headers
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return Err(ReaderError::new("unsupported_encoding"));
        }
        return Ok(response);
    }
    unreachable!("bounded redirect loop returns")
}

fn fixed_uri(value: &str) -> Result<Uri, ReaderError> {
    if value.len() > 8192 || value.contains('#') {
        return Err(ReaderError::new("destination_not_allowed"));
    }
    let uri = value
        .parse()
        .map_err(|_| ReaderError::new("destination_not_allowed"))?;
    validate_uri(&uri)?;
    Ok(uri)
}
fn validate_uri(uri: &Uri) -> Result<&str, ReaderError> {
    let host = uri
        .host()
        .ok_or_else(|| ReaderError::new("destination_not_allowed"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| ReaderError::new("destination_not_allowed"))?
        .as_str();
    if uri.scheme_str() != Some("https")
        || ![
            "api.github.com",
            "github.com",
            "release-assets.githubusercontent.com",
        ]
        .contains(&host)
        || (authority != host && authority != format!("{host}:443"))
    {
        return Err(ReaderError::new("destination_not_allowed"));
    }
    Ok(host)
}
fn redirect_uri(base: &Uri, location: &str) -> Result<Uri, ReaderError> {
    if location.starts_with('/') && !location.starts_with("//") {
        return fixed_uri(&format!(
            "https://{}{}",
            base.authority().expect("validated URI"),
            location
        ));
    }
    fixed_uri(location)
}
fn asset_uri(tag: &str, name: &str) -> Result<Uri, ReaderError> {
    Version::parse_tag(tag)?;
    if name != "parins-update.json"
        && !["x86_64", "aarch64"]
            .iter()
            .any(|arch| name == format!("parins-{tag}-linux-{arch}.bin"))
    {
        return Err(ReaderError::new("release_changed"));
    }
    fixed_uri(&format!(
        "https://github.com/paricafe/PariNS/releases/download/{tag}/{name}"
    ))
}
fn parse_release(bytes: &[u8]) -> Result<GithubRelease, ReaderError> {
    let mut release: GithubRelease =
        serde_json::from_slice(bytes).map_err(|_| ReaderError::new("invalid_release"))?;
    Version::parse_tag(&release.tag_name)?;
    if release.id == 0 || release.draft || release.prerelease {
        return Err(ReaderError::new("invalid_release"));
    }
    if let Some(body) = &mut release.body {
        let mut end = body.len().min(64 * 1024);
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    if release
        .published_at
        .as_ref()
        .is_some_and(|value| value.len() > 64)
    {
        return Err(ReaderError::new("invalid_release"));
    }
    Ok(release)
}
fn content_length(headers: &HeaderMap) -> Result<Option<u64>, ReaderError> {
    headers
        .get(header::CONTENT_LENGTH)
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| ReaderError::new("invalid_response"))
        })
        .transpose()
}
async fn collect(mut response: WireResponse, limit: usize) -> Result<Vec<u8>, ReaderError> {
    let length = content_length(&response.headers)?;
    if length.is_some_and(|n| n > limit as u64) {
        return Err(ReaderError::new("response_too_large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk?;
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(ReaderError::new("response_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if length.is_some_and(|n| n != bytes.len() as u64) {
        return Err(ReaderError::new("invalid_response"));
    }
    Ok(bytes)
}
async fn stream_binary(
    mut response: WireResponse,
    file: &mut tokio::fs::File,
    tuple: &DownloadTuple,
    progress: &mut impl FnMut(u64),
) -> Result<(), ReaderError> {
    if content_length(&response.headers)?.is_some_and(|n| n != tuple.size) {
        return Err(ReaderError::new("verification_failed"));
    }
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk?;
        if chunk.len() as u64 > tuple.size.saturating_sub(written) {
            return Err(ReaderError::new("response_too_large"));
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| ReaderError::new("file_write_failed"))?;
        hasher.update(&chunk);
        written += chunk.len() as u64;
        progress(written);
    }
    if written != tuple.size || format!("{:x}", hasher.finalize()) != tuple.sha256 {
        return Err(ReaderError::new("verification_failed"));
    }
    file.flush()
        .await
        .map_err(|_| ReaderError::new("file_write_failed"))?;
    Ok(())
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn http_error(status: StatusCode, headers: &HeaderMap, now: u64) -> ReaderError {
    let retry = headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<u64>()
                .ok()
                .map(|s| now.saturating_add(s))
                .or_else(|| {
                    httpdate::parse_http_date(v)
                        .ok()?
                        .duration_since(UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs())
                })
        });
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    ReaderError {
        code: if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
            "rate_limited"
        } else if status == StatusCode::NOT_FOUND {
            "release_incomplete"
        } else {
            "http_error"
        },
        retry_after_unix: retry.into_iter().chain(reset).max().filter(|t| *t > now),
    }
}

#[cfg(test)]
mod tests;
