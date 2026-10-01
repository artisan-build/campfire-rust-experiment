//! An S3-compatible object store for blobs, in the shape `ActiveStorage::Service::S3Service`
//! gives Rails: `upload`, `download`, `download_chunk`, `delete`, `delete_prefixed` and `exist?`.
//!
//! What it is NOT is Active Storage's `:amazon` service end to end: there are no presigned URLs
//! in a browser. The app keeps serving every blob itself behind its own signed route, so the only
//! thing that moved is where the bytes live. See `service.rs` for why.
//!
//! How it talks to the bucket, and why:
//!
//! * **Requests are presigned and then sent over plain HTTP.** `rusty_s3` signs a URL (SigV4,
//!   path- or virtual-host-style) and the whole credential lives in the query string, so nothing
//!   else has to agree about which headers are signed. It also means the requests carry *no*
//!   checksum header of their own: Cloudflare R2 — which is what a Laravel Cloud bucket is —
//!   rejects a `PUT` that carries both a `Content-MD5` and an `x-amz-checksum-*`, and the AWS SDK
//!   sends both by default. The integrity check Active Storage's `Content-MD5` buys is done here
//!   instead, against the source bytes, *before* the object is written (see [`S3Service::upload`]).
//! * **The client is blocking** (`ureq`). Every call into this crate already runs on
//!   `spawn_blocking`, because the file work (libvips, ffmpeg, checksums) must not hold the
//!   database writer; a blocking client keeps `crates/storage` free of Tokio.
//! * **A signed URL is never logged or put in an error.** It contains the access key id and the
//!   signature. Errors name the method, the object key and the status, and nothing else.

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rusty_s3::actions::S3Action;
use rusty_s3::{Bucket, Credentials, UrlStyle};

use crate::key::{checksum, checksum_file};
use crate::service::{Source, Stat};
use crate::{Error, Result};

/// How long a signed request stays valid. The URL is signed and sent in the same breath and never
/// leaves the process, so this only has to cover the clock skew between us and the bucket.
const SIGNED_FOR: Duration = Duration::from_secs(120);

/// The prefix blob objects live under, so that nothing else sharing the bucket can collide with
/// them. Laravel Cloud attaches one bucket per environment and the port already keeps the SQLite
/// database's Litestream replica in it under `campfire/` (see `main.go`).
pub const DEFAULT_PREFIX: &str = "blobs/";

#[derive(Clone)]
pub struct S3Service {
    name: String,
    prefix: String,
    bucket: Arc<Bucket>,
    /// Read from the environment and never written anywhere: not to a config file, not to a log,
    /// not into an error message.
    credentials: Arc<Credentials>,
    agent: ureq::Agent,
}

/// Deliberately hand-written: the derived one would print the credentials.
impl fmt::Debug for S3Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Service")
            .field("name", &self.name)
            .field("bucket", &self.bucket.name())
            .field("region", &self.bucket.region())
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl S3Service {
    /// The service Laravel Cloud's injected variables describe. Attaching a bucket to an
    /// environment is what makes Cloud put `AWS_BUCKET`, `AWS_ENDPOINT`/`AWS_ENDPOINT_URL`,
    /// `AWS_REGION`/`AWS_DEFAULT_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
    /// `AWS_USE_PATH_STYLE_ENDPOINT` in the environment, and they are the only place credentials
    /// are read from. `CAMPFIRE_STORAGE_S3_PREFIX` overrides [`DEFAULT_PREFIX`].
    pub fn from_env(name: impl Into<String>) -> Result<Self> {
        let var = |names: &[&str]| names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()));
        let required =
            |names: &[&str]| var(names).ok_or_else(|| Error::Service(format!("object storage: {} is not set", names.join(" or "))));

        let bucket = required(&["AWS_BUCKET"])?;
        let endpoint = required(&["AWS_ENDPOINT_URL", "AWS_ENDPOINT"])?;
        let region = var(&["AWS_REGION", "AWS_DEFAULT_REGION"]).unwrap_or_else(|| "auto".into());
        let access_key_id = required(&["AWS_ACCESS_KEY_ID"])?;
        let secret_access_key = required(&["AWS_SECRET_ACCESS_KEY"])?;
        let session_token = var(&["AWS_SESSION_TOKEN"]);
        // Cloud sets it to "true" for R2, whose endpoint is per-account rather than per-bucket.
        let path_style = var(&["AWS_USE_PATH_STYLE_ENDPOINT"]).is_none_or(|v| truthy(&v));
        let prefix = var(&["CAMPFIRE_STORAGE_S3_PREFIX"]).unwrap_or_else(|| DEFAULT_PREFIX.into());

        let credentials = match session_token {
            Some(token) => Credentials::new_with_token(access_key_id, secret_access_key, token),
            None => Credentials::new(access_key_id, secret_access_key),
        };
        Self::new(name, &endpoint, &bucket, &region, path_style, &prefix, credentials)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        endpoint: &str,
        bucket: &str,
        region: &str,
        path_style: bool,
        prefix: &str,
        credentials: Credentials,
    ) -> Result<Self> {
        let endpoint = endpoint.parse().map_err(|e| Error::Service(format!("object storage: bad endpoint: {e}")))?;
        let style = if path_style { UrlStyle::Path } else { UrlStyle::VirtualHost };
        let bucket = Bucket::new(endpoint, style, bucket.to_string(), region.to_string())
            .map_err(|e| Error::Service(format!("object storage: bad bucket: {e}")))?;
        // A 404 and a 416 are answers, not transport failures: they're read from the status.
        let config = ureq::Agent::config_builder().http_status_as_error(false).build();
        Ok(Self {
            name: name.into(),
            prefix: prefix.to_string(),
            bucket: Arc::new(bucket),
            credentials: Arc::new(credentials),
            agent: ureq::Agent::new_with_config(config),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The object's name in the bucket: the blob key under the blob prefix.
    pub fn object(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    pub fn describe(&self) -> String {
        format!(
            "s3 service `{}` in bucket {} ({}, {}) under {:?}",
            self.name,
            self.bucket.name(),
            self.bucket.base_url().host_str().unwrap_or("?"),
            self.bucket.region(),
            self.prefix
        )
    }

    /// `upload(key, io, checksum:)`. The checksum is verified against the *source* first, so a
    /// mismatch writes nothing at all — Active Storage's disk and S3 services both let the bad
    /// bytes land and then undo it (`Content-MD5` for S3, delete-on-mismatch for disk), which
    /// isn't an option here: a `Content-MD5` alongside the SDK's own checksum is exactly what
    /// Cloudflare R2 rejects, and we send neither.
    pub fn upload(&self, key: &str, source: Source<'_>, expected: Option<&str>) -> Result<()> {
        if let Some(expected) = expected {
            let actual = match source {
                Source::Bytes(bytes) => checksum(bytes),
                Source::File(path) => checksum_file(path)?,
            };
            if actual != expected {
                return Err(Error::Integrity);
            }
        }
        let object = self.object(key);
        let url = self.bucket.put_object(Some(&self.credentials), &object).sign(SIGNED_FOR);
        let request = self.agent.put(url.as_str());
        let response = match source {
            Source::Bytes(bytes) => request.send(bytes),
            Source::File(path) => request.send(std::fs::File::open(path)?),
        };
        self.ok(response, "PUT", &object)?;
        Ok(())
    }

    pub fn download(&self, key: &str) -> Result<Vec<u8>> {
        let object = self.object(key);
        let mut body = self.get(&object, None)?;
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn download_to(&self, key: &str, dest: &Path) -> Result<()> {
        let object = self.object(key);
        let mut body = self.get(&object, None)?;
        let mut file = std::fs::File::create(dest)?;
        std::io::copy(&mut body, &mut file)?;
        std::io::Write::flush(&mut file)?;
        Ok(())
    }

    /// `download_chunk(key, range)`, as a reader so a byte-range response streams.
    pub fn open_range(&self, key: &str, start: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        if len == 0 {
            return Ok(Box::new(std::io::empty()));
        }
        let object = self.object(key);
        let end = start + len - 1;
        self.get(&object, Some(&format!("bytes={start}-{end}")))
    }

    /// `delete(key)`, which is a no-op when the object is already gone.
    pub fn delete(&self, key: &str) -> Result<()> {
        let object = self.object(key);
        let url = self.bucket.delete_object(Some(&self.credentials), &object).sign(SIGNED_FOR);
        self.ok(self.agent.delete(url.as_str()).call(), "DELETE", &object)?;
        Ok(())
    }

    /// `delete_prefixed(prefix)`: every object whose key starts with `prefix`. One `DELETE` each
    /// rather than a batch `POST ?delete`, which would need `rusty_s3`'s `full` feature; this is
    /// only reached for the legacy untracked `variants/<key>/` tree, which holds a handful of
    /// objects at most.
    pub fn delete_prefixed(&self, prefix: &str) -> Result<()> {
        for (key, _) in self.list(prefix)? {
            self.delete(&key)?;
        }
        Ok(())
    }

    pub fn exist(&self, key: &str) -> bool {
        match self.stat(key) {
            Ok(_) => true,
            Err(Error::FileNotFound) => false,
            Err(error) => {
                tracing::warn!(key, %error, "object storage: HEAD failed");
                false
            }
        }
    }

    /// `HEAD` the object: `Content-Length` and `Last-Modified`.
    pub fn stat(&self, key: &str) -> Result<Stat> {
        let object = self.object(key);
        let url = self.bucket.head_object(Some(&self.credentials), &object).sign(SIGNED_FOR);
        let response = self.ok(self.agent.head(url.as_str()).call(), "HEAD", &object)?;
        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let size = header("content-length")
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(|| Error::Service(format!("object storage: HEAD {object}: no Content-Length")))?;
        let modified = header("last-modified")
            .as_deref()
            .and_then(parse_http_date)
            .ok_or_else(|| Error::Service(format!("object storage: HEAD {object}: no usable Last-Modified")))?;
        Ok(Stat { size, modified })
    }

    /// The keys (as blob keys, with the prefix stripped) and sizes under `prefix`.
    pub fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let full = self.object(prefix);
        let mut found = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
            action.with_prefix(full.clone());
            if let Some(token) = &token {
                action.with_continuation_token(token.clone());
            }
            let url = action.sign(SIGNED_FOR);
            let response = self.ok(self.agent.get(url.as_str()).call(), "GET ?list-type=2", &full)?;
            let xml = response
                .into_body()
                .into_with_config()
                .limit(u64::MAX)
                .read_to_string()
                .map_err(|e| Error::Service(format!("object storage: list {full}: {e}")))?;
            let parsed = rusty_s3::actions::ListObjectsV2::parse_response(&xml)
                .map_err(|e| Error::Service(format!("object storage: list {full}: {e}")))?;
            for item in parsed.contents {
                if let Some(key) = item.key.strip_prefix(&self.prefix) {
                    found.push((key.to_string(), item.size));
                }
            }
            match parsed.next_continuation_token {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        Ok(found)
    }

    /// A `GET`, optionally ranged, as a reader over the body.
    fn get(&self, object: &str, range: Option<&str>) -> Result<Box<dyn Read + Send>> {
        let url = self.bucket.get_object(Some(&self.credentials), object).sign(SIGNED_FOR);
        // `Range` is not part of the signature: S3 and R2 both honour it on a presigned GET.
        let mut request = self.agent.get(url.as_str());
        if let Some(range) = range {
            request = request.header("range", range);
        }
        let response = self.ok(request.call(), "GET", object)?;
        Ok(Box::new(response.into_body().into_with_config().limit(u64::MAX).reader()))
    }

    /// Turns a transport failure or a non-2xx status into this crate's errors. A 404 becomes
    /// [`Error::FileNotFound`], the way `DiskService` reports a missing file, so the callers'
    /// `rescue ActiveStorage::FileNotFoundError` paths still work. Everything else — a 403 very
    /// much included, because a credential or clock problem must never read as "no such blob" —
    /// stays an error. The URL is never in the message: it carries the credential.
    fn ok(
        &self,
        response: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        method: &str,
        object: &str,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let response = response.map_err(|e| Error::Service(format!("object storage: {method} {object}: {e}")))?;
        let status = response.status().as_u16();
        match status {
            200..=299 => Ok(response),
            404 => Err(Error::FileNotFound),
            _ => Err(Error::Service(format!("object storage: {method} {object}: HTTP {status}"))),
        }
    }
}

fn truthy(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// An HTTP-date (`Sun, 06 Nov 1994 08:49:37 GMT`), which is what `Last-Modified` is from S3 and
/// R2. The day name is dropped rather than parsed: it is redundant with the date, and a store
/// that got it wrong would otherwise make every blob look missing.
fn parse_http_date(value: &str) -> Option<jiff::Timestamp> {
    let date = value.trim().split_once(", ").map_or(value.trim(), |(_, rest)| rest);
    let civil = jiff::civil::DateTime::strptime("%d %b %Y %H:%M:%S GMT", date).ok()?;
    civil.to_zoned(jiff::tz::TimeZone::UTC).ok().map(|zoned| zoned.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> S3Service {
        S3Service::new(
            "local",
            "https://account.r2.cloudflarestorage.com",
            "campfire-rust",
            "auto",
            true,
            DEFAULT_PREFIX,
            Credentials::new("AKIAEXAMPLE", "secret"),
        )
        .unwrap()
    }

    #[test]
    fn an_object_is_the_blob_key_under_the_blob_prefix() {
        assert_eq!(service().object("abcdefghijklmnopqrstuvwxyz12"), "blobs/abcdefghijklmnopqrstuvwxyz12");
        // Litestream's replica (`campfire/...`, see main.go) can never collide with a blob.
        assert!(service().object("anything").starts_with("blobs/"));
    }

    #[test]
    fn neither_debug_nor_describe_leaks_the_credentials() {
        let service = service();
        for rendered in [format!("{service:?}"), service.describe()] {
            assert!(!rendered.contains("secret"), "{rendered}");
            assert!(!rendered.contains("AKIAEXAMPLE"), "{rendered}");
        }
    }

    #[test]
    fn path_style_puts_the_bucket_in_the_path() {
        let path_style = service();
        assert_eq!(path_style.bucket.base_url().as_str(), "https://account.r2.cloudflarestorage.com/campfire-rust/");
        let vhost = S3Service::new(
            "local",
            "https://s3.us-east-2.amazonaws.com",
            "campfire-rust",
            "us-east-2",
            false,
            DEFAULT_PREFIX,
            Credentials::new("AKIAEXAMPLE", "secret"),
        )
        .unwrap();
        assert_eq!(vhost.bucket.base_url().as_str(), "https://campfire-rust.s3.us-east-2.amazonaws.com/");
    }

    #[test]
    fn http_dates_parse_the_way_last_modified_is_written() {
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some("1994-11-06T08:49:37Z".parse().unwrap()));
        assert_eq!(parse_http_date("Thu, 01 Oct 2026 12:00:00 GMT"), Some("2026-10-01T12:00:00Z".parse().unwrap()));
        // A wrong day name must not make the object look missing: Oct 1 2026 is a Thursday.
        assert_eq!(parse_http_date("Wed, 01 Oct 2026 12:00:00 GMT"), Some("2026-10-01T12:00:00Z".parse().unwrap()));
        assert_eq!(parse_http_date("nonsense"), None);
        assert_eq!(parse_http_date(""), None);
    }

    #[test]
    fn an_upload_whose_checksum_does_not_match_the_source_is_refused_before_anything_is_sent() {
        // The service points at a host that does not resolve, so reaching the network at all
        // would be a transport error rather than `Integrity`.
        let service = S3Service::new(
            "local",
            "https://127.0.0.1:1",
            "campfire-rust",
            "auto",
            true,
            DEFAULT_PREFIX,
            Credentials::new("AKIAEXAMPLE", "secret"),
        )
        .unwrap();
        let refused = service.upload("key", Source::Bytes(b"bytes"), Some(&checksum(b"other bytes")));
        assert!(matches!(refused, Err(Error::Integrity)), "{refused:?}");
    }

    #[test]
    fn a_signed_url_carries_the_credential_in_the_query_so_no_header_has_to_be_signed() {
        let service = service();
        let url = service.bucket.put_object(Some(&service.credentials), &service.object("k")).sign(SIGNED_FOR);
        assert!(url.query().unwrap().contains("X-Amz-Credential=AKIAEXAMPLE"), "{url}");
        assert!(url.query().unwrap().contains("X-Amz-Signature="), "{url}");
        assert_eq!(url.path(), "/campfire-rust/blobs/k");
    }
}
