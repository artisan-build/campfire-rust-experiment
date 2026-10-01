//! The byte store behind the blobs: Active Storage's service layer, narrowed to the operations
//! Campfire reaches, with the two services the port ships — the local disk ([`DiskService`], what
//! `config/storage.yml` configures) and an S3-compatible bucket ([`S3Service`]).
//!
//! Active Storage hides the difference behind `ActiveStorage::Blob.service`; this crate had
//! "one disk service" hard-wired instead, so [`Service`] is the layer that was missing. It is an
//! enum rather than a trait object because there are exactly two of them, both `Clone`, and the
//! callers want a plain value they can put in an `Arc` and move into a blocking task.
//!
//! Three things deliberately do NOT vary by service, so that putting blobs in a bucket changes
//! nothing about who may read them:
//!
//! * Blob URLs. Active Storage's `:amazon` service hands out presigned S3 URLs; this one keeps
//!   the app's own signed `/rails/active_storage/disk/...` route for both services and streams
//!   the bytes through the app, so the signature, its purpose, its payload and its expiry are the
//!   ones `DiskController` already enforced. See `url_path`.
//! * Direct uploads. The `PUT` still lands on the app (behind
//!   `require_active_storage_authentication`), which writes to the service. No browser ever holds
//!   a credential or a presigned PUT.
//! * The key. An object's name in the bucket is the blob's key under one prefix, so nothing has
//!   to be migrated and the keys in the database mean the same thing in both services.

use std::io::Read;
use std::path::{Path, PathBuf};

use rails_compat::MessageVerifier;

use crate::disk::{self, DiskService};
use crate::filename::Filename;
use crate::s3::S3Service;
use crate::{Error, Result};

/// Where an object's bytes come from on the way in. Active Storage's `upload(key, io:)` takes an
/// IO; a path or a slice instead lets the S3 service send a file as a sized body rather than
/// buffering it, and lets the disk service copy without going through user space.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    Bytes(&'a [u8]),
    File(&'a Path),
}

/// What a conditional GET and a byte range need about an object: `File#size` and `File#mtime` on
/// disk, `Content-Length` and `Last-Modified` in a bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stat {
    pub size: u64,
    pub modified: jiff::Timestamp,
}

#[derive(Clone, Debug)]
pub enum Service {
    Disk(DiskService),
    S3(S3Service),
}

impl Service {
    pub fn disk(root: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        Self::Disk(DiskService::new(root, name))
    }

    /// `service.name`, which goes into every blob row's `service_name` and into the signed disk
    /// URLs. Campfire's only configured service is `local` (`config/storage.yml`), and keeping
    /// that name for the bucket too is what makes existing rows and existing signed URLs valid:
    /// `DiskController` compares the name in the URL with the service it is serving from.
    pub fn name(&self) -> &str {
        match self {
            Self::Disk(disk) => disk.name(),
            Self::S3(s3) => s3.name(),
        }
    }

    /// The file on disk holding `key`, for the callers that can take a shortcut (a `send_file`
    /// response, a test reading the bytes back). `None` for a bucket: there is no local file.
    pub fn local_path(&self, key: &str) -> Option<PathBuf> {
        match self {
            Self::Disk(disk) => Some(disk.path_for(key)),
            Self::S3(_) => None,
        }
    }

    /// `upload(key, io, checksum:)`: the bytes, then the integrity check.
    pub fn upload(&self, key: &str, source: Source<'_>, checksum: Option<&str>) -> Result<()> {
        match self {
            Self::Disk(disk) => disk.upload(key, source, checksum),
            Self::S3(s3) => s3.upload(key, source, checksum),
        }
    }

    pub fn download(&self, key: &str) -> Result<Vec<u8>> {
        match self {
            Self::Disk(disk) => disk.download(key),
            Self::S3(s3) => s3.download(key),
        }
    }

    /// The whole object written to `dest`, which already exists (it is a tempfile). This is what
    /// `blob.open` needs: libvips, ffmpeg and the analyzers all want a real local file.
    pub fn download_to(&self, key: &str, dest: &Path) -> Result<()> {
        match self {
            Self::Disk(disk) => disk.download_to(key, dest),
            Self::S3(s3) => s3.download_to(key, dest),
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        match self {
            Self::Disk(disk) => disk.delete(key),
            Self::S3(s3) => s3.delete(key),
        }
    }

    /// `delete_prefixed(prefix)`: everything whose key starts with `prefix`.
    pub fn delete_prefixed(&self, prefix: &str) -> Result<()> {
        match self {
            Self::Disk(disk) => disk.delete_prefixed(prefix),
            Self::S3(s3) => s3.delete_prefixed(prefix),
        }
    }

    pub fn exist(&self, key: &str) -> bool {
        match self {
            Self::Disk(disk) => disk.exist(key),
            Self::S3(s3) => s3.exist(key),
        }
    }

    /// [`Stat`] for `key`, or [`Error::FileNotFound`] when there is no such object.
    pub fn stat(&self, key: &str) -> Result<Stat> {
        match self {
            Self::Disk(disk) => disk.stat(key),
            Self::S3(s3) => s3.stat(key),
        }
    }

    /// A blocking reader over `len` bytes of `key` from `start`. The caller is responsible for
    /// asking for a range the object actually has (it has just [`Self::stat`]ed it).
    pub fn open_range(&self, key: &str, start: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        match self {
            Self::Disk(disk) => disk.open_range(key, start, len),
            Self::S3(s3) => s3.open_range(key, start, len),
        }
    }

    /// The path of `service.url(key, expires_in:, filename:, content_type:, disposition:)`. The
    /// same signed app route for both services — see this module's note on the access model.
    pub fn url_path(
        &self,
        verifier: &MessageVerifier,
        key: &str,
        expires_at: Option<jiff::Timestamp>,
        filename: &Filename,
        content_type: Option<&str>,
        disposition: &str,
    ) -> String {
        disk::url_path(verifier, self.name(), key, expires_at, filename, content_type, disposition)
    }

    /// The path of `url_for_direct_upload`.
    pub fn url_path_for_direct_upload(
        &self,
        verifier: &MessageVerifier,
        key: &str,
        expires_at: jiff::Timestamp,
        content_type: Option<&str>,
        content_length: i64,
        checksum: &str,
    ) -> String {
        disk::url_path_for_direct_upload(verifier, self.name(), key, expires_at, content_type, content_length, checksum)
    }

    /// The keys and sizes of the objects under `prefix`, for diagnostics (`campfire storage:list`).
    /// The disk service walks its own tree; a bucket lists itself.
    pub fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        match self {
            Self::Disk(disk) => disk.list(prefix),
            Self::S3(s3) => s3.list(prefix),
        }
    }

    /// One line naming the service for the boot log. Never includes a credential: for a bucket
    /// that is the bucket name, its endpoint host and the key prefix, and nothing else.
    pub fn describe(&self) -> String {
        match self {
            Self::Disk(disk) => format!("disk service `{}` at {}", disk.name(), disk.root().display()),
            Self::S3(s3) => s3.describe(),
        }
    }
}

/// Picks the service from the environment, the way `config/storage.yml` plus `RAILS_ENV` would.
///
/// `CAMPFIRE_STORAGE_SERVICE` decides when it is set (`disk` or `s3`); otherwise a bucket is used
/// whenever one is attached, which on Laravel Cloud means `AWS_BUCKET` and the rest of the `AWS_*`
/// group are injected into the environment. Credentials are only ever read from there
/// ([`S3Service::from_env`]); nothing in this crate accepts them from a config file or an argument.
pub fn from_env(disk_root: impl Into<PathBuf>) -> Result<Service> {
    let requested = std::env::var("CAMPFIRE_STORAGE_SERVICE").ok().map(|v| v.trim().to_ascii_lowercase());
    let bucket_attached = std::env::var_os("AWS_BUCKET").is_some_and(|v| !v.is_empty());
    match requested.as_deref() {
        Some("s3") => Ok(Service::S3(S3Service::from_env("local")?)),
        Some(other) if other != "disk" && other != "local" => {
            Err(Error::Service(format!("unknown CAMPFIRE_STORAGE_SERVICE {other:?}; expected `disk` or `s3`")))
        }
        None if bucket_attached => Ok(Service::S3(S3Service::from_env("local")?)),
        _ => Ok(Service::disk(disk_root, "local")),
    }
}
