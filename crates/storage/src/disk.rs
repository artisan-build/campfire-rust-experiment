//! `ActiveStorage::Service::DiskService`: files at `<root>/<key[0..2]>/<key[2..4]>/<key>`, and the
//! signed disk URLs (`/rails/active_storage/disk/:encoded_key/*filename`) and upload tokens
//! (`PUT /rails/active_storage/disk/:encoded_token`).

use std::fs;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

use rails_compat::MessageVerifier;

use crate::disposition::{content_disposition_with, escape_path, escape_segment};
use crate::filename::Filename;
use crate::json::Json;
use crate::key::checksum_file;
use crate::service::{Source, Stat};
use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct DiskService {
    root: PathBuf,
    name: String,
}

/// The payload of a disk URL's `encoded_key` (purpose "blob_key").
#[derive(Clone, Debug, PartialEq)]
pub struct DiskKey {
    pub key: String,
    pub disposition: String,
    pub content_type: Option<String>,
    pub service_name: String,
}

/// The payload of a direct-upload `encoded_token` (purpose "blob_token").
#[derive(Clone, Debug, PartialEq)]
pub struct DiskToken {
    pub key: String,
    pub content_type: Option<String>,
    pub content_length: i64,
    pub checksum: String,
    pub service_name: String,
}

impl DiskService {
    /// Campfire's `local` service: `root: Rails.root.join("storage", "files")` (config/storage.yml).
    pub fn new(root: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        Self { root: root.into(), name: name.into() }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, key: &str) -> PathBuf {
        self.root.join(folder_for(key)).join(key)
    }

    /// `upload(key, io, checksum:)`: write, then verify the MD5 and delete on mismatch.
    pub fn upload(&self, key: &str, source: Source<'_>, checksum: Option<&str>) -> Result<()> {
        let path = self.make_path_for(key)?;
        match source {
            Source::Bytes(bytes) => {
                let mut file = fs::File::create(&path)?;
                file.write_all(bytes)?;
                file.flush()?;
            }
            Source::File(from) => {
                fs::copy(from, &path)?;
            }
        }
        if let Some(checksum) = checksum {
            self.ensure_integrity_of(key, checksum)?;
        }
        Ok(())
    }

    pub fn download(&self, key: &str) -> Result<Vec<u8>> {
        fs::read(self.path_for(key)).map_err(not_found)
    }

    /// The whole file copied over `dest`, which already exists.
    pub fn download_to(&self, key: &str, dest: &Path) -> Result<()> {
        fs::copy(self.path_for(key), dest).map_err(not_found)?;
        Ok(())
    }

    /// `File#size` and `File#mtime`, which is what `Rack::Files` serves conditional GETs from.
    pub fn stat(&self, key: &str) -> Result<Stat> {
        let metadata = fs::metadata(self.path_for(key)).map_err(not_found)?;
        let modified = jiff::Timestamp::try_from(metadata.modified()?).unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        Ok(Stat { size: metadata.len(), modified })
    }

    /// `download_chunk(key, range)`, as a reader so a byte-range response streams from disk.
    pub fn open_range(&self, key: &str, start: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        let mut file = fs::File::open(self.path_for(key)).map_err(not_found)?;
        file.seek(io::SeekFrom::Start(start))?;
        Ok(Box::new(file.take(len)))
    }

    /// The keys and sizes of the files under `prefix`, for diagnostics. Keys are two levels deep
    /// (`folder_for`), so this walks the tree rather than one directory.
    pub fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let mut found = Vec::new();
        walk(&self.root, prefix, &mut found)?;
        found.sort();
        Ok(found)
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        match fs::remove_file(self.path_for(key)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// `delete_prefixed(prefix)`: `rm_rf` everything matching `path_for("#{prefix}*")`.
    pub fn delete_prefixed(&self, prefix: &str) -> Result<()> {
        let pattern = self.path_for(prefix);
        let pattern = pattern.to_string_lossy();
        let (dir, stem) = pattern.rsplit_once('/').unwrap_or((".", &pattern));
        let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(stem) {
                let path = entry.path();
                if path.is_dir() { fs::remove_dir_all(path)? } else { fs::remove_file(path)? }
            }
        }
        Ok(())
    }

    pub fn exist(&self, key: &str) -> bool {
        self.path_for(key).exists()
    }

    fn make_path_for(&self, key: &str) -> Result<PathBuf> {
        let path = self.path_for(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(path)
    }

    fn ensure_integrity_of(&self, key: &str, checksum: &str) -> Result<()> {
        if checksum_file(&self.path_for(key))? != checksum {
            self.delete(key)?;
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

/// The path of `service.url(key, expires_in:, filename:, content_type:, disposition:)` (the caller
/// prefixes `ActiveStorage::Current.url_options`' protocol and host).
///
/// A free function taking the service name, not a `DiskService` method, because every service
/// serves its blobs through this one signed app route — see `service.rs`.
pub fn url_path(
    verifier: &MessageVerifier,
    service_name: &str,
    key: &str,
    expires_at: Option<jiff::Timestamp>,
    filename: &Filename,
    content_type: Option<&str>,
    disposition: &str,
) -> String {
    let sanitized = filename.sanitized();
    let payload = Json::Object(vec![
        ("key".into(), key.into()),
        ("disposition".into(), content_disposition_with(disposition, &sanitized).into()),
        ("content_type".into(), content_type.map_or(Json::Null, Json::from)),
        ("service_name".into(), service_name.into()),
    ]);
    let encoded_key = verifier.generate_raw(&payload.encode(), Some("blob_key"), expires_at);
    format!("/rails/active_storage/disk/{}/{}", escape_segment(&encoded_key), escape_path(&sanitized))
}

/// The path of `url_for_direct_upload`.
pub fn url_path_for_direct_upload(
    verifier: &MessageVerifier,
    service_name: &str,
    key: &str,
    expires_at: jiff::Timestamp,
    content_type: Option<&str>,
    content_length: i64,
    checksum: &str,
) -> String {
    let payload = Json::Object(vec![
        ("key".into(), key.into()),
        ("content_type".into(), content_type.map_or(Json::Null, Json::from)),
        ("content_length".into(), content_length.into()),
        ("checksum".into(), checksum.into()),
        ("service_name".into(), service_name.into()),
    ]);
    let token = verifier.generate_raw(&payload.encode(), Some("blob_token"), Some(expires_at));
    format!("/rails/active_storage/disk/{}", escape_segment(&token))
}

/// `DiskController#decode_verified_key`.
pub fn decode_verified_key(verifier: &MessageVerifier, encoded_key: &str, now: jiff::Timestamp) -> Option<DiskKey> {
    let data = Json::parse(&verifier.verify_raw(encoded_key, Some("blob_key"), now).ok()?).ok()?;
    Some(DiskKey {
        key: data.get("key")?.as_str()?.to_string(),
        disposition: data.get("disposition")?.as_str()?.to_string(),
        content_type: data.get("content_type").and_then(Json::as_str).map(str::to_string),
        service_name: data.get("service_name")?.as_str()?.to_string(),
    })
}

/// `DiskController#decode_verified_token`.
pub fn decode_verified_token(verifier: &MessageVerifier, encoded_token: &str, now: jiff::Timestamp) -> Option<DiskToken> {
    let data = Json::parse(&verifier.verify_raw(encoded_token, Some("blob_token"), now).ok()?).ok()?;
    Some(DiskToken {
        key: data.get("key")?.as_str()?.to_string(),
        content_type: data.get("content_type").and_then(Json::as_str).map(str::to_string),
        content_length: data.get("content_length")?.as_i64()?,
        checksum: data.get("checksum")?.as_str()?.to_string(),
        service_name: data.get("service_name")?.as_str()?.to_string(),
    })
}

/// `[key[0..1], key[2..3]].join("/")`.
fn folder_for(key: &str) -> String {
    let a = key.get(0..2).unwrap_or(key);
    let b = key.get(2..4).or_else(|| key.get(2..)).unwrap_or("");
    format!("{a}/{b}")
}

/// Every file under `dir` whose name starts with `prefix`. Names, not paths: a key's file is
/// named after the whole key, and its two directories are derived from it (`folder_for`).
fn walk(dir: &Path, prefix: &str, found: &mut Vec<(String, u64)>) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, prefix, found)?;
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with(prefix)
        {
            found.push((name.to_string(), entry.metadata()?.len()));
        }
    }
    Ok(())
}

fn not_found(e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::NotFound { Error::FileNotFound } else { e.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::checksum;

    #[test]
    fn an_upload_with_a_checksum_is_verified() {
        let root = tempfile::tempdir().unwrap();
        let service = DiskService::new(root.path(), "local");
        service.upload("matching", Source::Bytes(b"bytes"), Some(&checksum(b"bytes"))).unwrap();
        assert_eq!(service.download("matching").unwrap(), b"bytes");

        let mismatched = service.upload("mismatched", Source::Bytes(b"bytes"), Some(&checksum(b"other bytes")));
        assert!(matches!(mismatched, Err(Error::Integrity)), "{mismatched:?}");
        assert!(!service.exist("mismatched"));

        service.upload("unchecked", Source::Bytes(b"bytes"), None).unwrap();
        assert_eq!(service.download("unchecked").unwrap(), b"bytes");
    }

    #[test]
    fn a_range_reads_only_its_own_bytes() {
        let root = tempfile::tempdir().unwrap();
        let service = DiskService::new(root.path(), "local");
        service.upload("ranged", Source::Bytes(b"0123456789"), None).unwrap();
        let mut read = Vec::new();
        service.open_range("ranged", 3, 4).unwrap().read_to_end(&mut read).unwrap();
        assert_eq!(read, b"3456");
        assert_eq!(service.stat("ranged").unwrap().size, 10);
        assert!(matches!(service.stat("absent"), Err(Error::FileNotFound)));
    }

    #[test]
    fn listing_finds_the_keys_under_a_prefix() {
        let root = tempfile::tempdir().unwrap();
        let service = DiskService::new(root.path(), "local");
        service.upload("aa11key", Source::Bytes(b"x"), None).unwrap();
        service.upload("aa11other", Source::Bytes(b"yy"), None).unwrap();
        service.upload("zz99key", Source::Bytes(b"zzz"), None).unwrap();
        assert_eq!(service.list("aa11").unwrap(), vec![("aa11key".into(), 1), ("aa11other".into(), 2)]);
        assert_eq!(service.list("").unwrap().len(), 3);
    }
}
