//! The S3 service against a real S3 API, which is the only thing that proves it: presigning, the
//! wire format and what an object store does with a `Range`, a missing key or a `DELETE` of
//! something that was never there are exactly the parts a fake would have to guess at.
//!
//! The server is **MinIO in Docker**, not an in-memory fake. `parity/bin/s3 up` starts it and
//! exports the variables below; without them these tests pass without running and say so on
//! stderr, the way the app's integration tests treat missing seed data.
//!
//!   parity/bin/s3 up
//!   eval "$(parity/bin/s3 env)" && cargo test -p campfire_storage --test s3
//!
//! `CAMPFIRE_REQUIRE_S3=1` turns a missing server into a failure instead.

use std::io::Read;

use campfire_storage::service::{Source, Stat};
use campfire_storage::{Error, Filename, S3Service, Service, Storage};
use rusty_s3::Credentials;

/// A prefix unique to this run, so tests are isolated from each other *and* from the objects an
/// earlier run of the same test left in the bucket (MinIO keeps them until the container goes).
fn run_id() -> &'static str {
    static RUN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RUN.get_or_init(|| format!("{}-{}", std::process::id(), jiff::Timestamp::now().as_millisecond()))
}

/// A service against the test bucket under a prefix of this test's own, or `None` when no server
/// was configured. Each test gets its own prefix so they can run in parallel in one bucket.
fn service(prefix: &str) -> Option<S3Service> {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let Some(endpoint) = var("CAMPFIRE_TEST_S3_ENDPOINT") else {
        assert!(
            std::env::var_os("CAMPFIRE_REQUIRE_S3").is_none(),
            "CAMPFIRE_REQUIRE_S3 is set but CAMPFIRE_TEST_S3_ENDPOINT is not: start MinIO with parity/bin/s3 up"
        );
        eprintln!("note: no CAMPFIRE_TEST_S3_ENDPOINT, skipping the S3 service tests (parity/bin/s3 up)");
        return None;
    };
    let credentials = Credentials::new(
        var("CAMPFIRE_TEST_S3_ACCESS_KEY_ID").expect("CAMPFIRE_TEST_S3_ACCESS_KEY_ID"),
        var("CAMPFIRE_TEST_S3_SECRET_ACCESS_KEY").expect("CAMPFIRE_TEST_S3_SECRET_ACCESS_KEY"),
    );
    Some(
        S3Service::new(
            "local",
            &endpoint,
            &var("CAMPFIRE_TEST_S3_BUCKET").unwrap_or_else(|| "campfire-test".into()),
            &var("CAMPFIRE_TEST_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
            true,
            &format!("{}/{prefix}/", run_id()),
            credentials,
        )
        .unwrap(),
    )
}

/// 28 base36 characters, the shape of a real blob key.
fn key(suffix: &str) -> String {
    format!("{suffix:0<28}")
}

#[test]
fn bytes_go_up_and_come_back_whole() {
    let Some(service) = service("roundtrip") else { return };
    let key = key("roundtrip");
    let data: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();

    service.upload(&key, Source::Bytes(&data), None).unwrap();
    assert_eq!(service.download(&key).unwrap(), data);
    assert!(service.exist(&key));

    let Stat { size, modified } = service.stat(&key).unwrap();
    assert_eq!(size, 100_000);
    // The object was written seconds ago, not at the epoch and not in the future.
    let age = jiff::Timestamp::now().as_second() - modified.as_second();
    assert!((0..600).contains(&age), "last-modified was {modified}");
}

#[test]
fn a_file_is_sent_as_a_sized_body_rather_than_buffered() {
    let Some(service) = service("fromfile") else { return };
    let key = key("fromfile");
    let data: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), &data).unwrap();

    service.upload(&key, Source::File(file.path()), None).unwrap();
    assert_eq!(service.stat(&key).unwrap().size, 300_000);
    assert_eq!(service.download(&key).unwrap(), data);
}

#[test]
fn a_range_reads_only_its_own_bytes() {
    let Some(service) = service("ranges") else { return };
    let key = key("ranges");
    service.upload(&key, Source::Bytes(b"0123456789"), None).unwrap();

    let read = |start, len| {
        let mut buf = Vec::new();
        service.open_range(&key, start, len).unwrap().read_to_end(&mut buf).unwrap();
        buf
    };
    assert_eq!(read(0, 10), b"0123456789");
    assert_eq!(read(3, 4), b"3456");
    assert_eq!(read(9, 1), b"9");
    assert_eq!(read(0, 0), b"");
}

#[test]
fn an_upload_whose_checksum_does_not_match_writes_nothing() {
    let Some(service) = service("integrity") else { return };
    let key = key("integrity");
    let refused = service.upload(&key, Source::Bytes(b"bytes"), Some(&campfire_storage::key::checksum(b"other")));
    assert!(matches!(refused, Err(Error::Integrity)), "{refused:?}");
    assert!(!service.exist(&key), "a refused upload must not leave an object behind");

    service.upload(&key, Source::Bytes(b"bytes"), Some(&campfire_storage::key::checksum(b"bytes"))).unwrap();
    assert_eq!(service.download(&key).unwrap(), b"bytes");
}

#[test]
fn a_missing_object_is_file_not_found_everywhere() {
    let Some(service) = service("missing") else { return };
    let key = key("missing");
    assert!(!service.exist(&key));
    assert!(matches!(service.stat(&key), Err(Error::FileNotFound)), "stat");
    assert!(matches!(service.download(&key), Err(Error::FileNotFound)), "download");
    let dest = tempfile::NamedTempFile::new().unwrap();
    assert!(matches!(service.download_to(&key, dest.path()), Err(Error::FileNotFound)), "download_to");
    // `DiskService#delete` is `File.delete rescue nil`, so this one is a no-op, not an error.
    service.delete(&key).unwrap();
}

#[test]
fn deleting_removes_the_object_and_the_prefixed_tree() {
    let Some(service) = service("deletes") else { return };
    let key = key("deletes");
    service.upload(&key, Source::Bytes(b"gone soon"), None).unwrap();
    service.delete(&key).unwrap();
    assert!(!service.exist(&key));

    // `Blob#delete`'s second half: the legacy untracked variants under `variants/<key>/`.
    for name in ["variants/abc/one", "variants/abc/two", "variants/other/three"] {
        service.upload(name, Source::Bytes(b"x"), None).unwrap();
    }
    service.delete_prefixed("variants/abc/").unwrap();
    assert!(!service.exist("variants/abc/one"));
    assert!(!service.exist("variants/abc/two"));
    assert!(service.exist("variants/other/three"), "a sibling prefix must survive");
}

#[test]
fn listing_sees_every_object_under_the_prefix() {
    let Some(service) = service("listing") else { return };
    for i in 0..3 {
        service.upload(&key(&format!("listing{i}")), Source::Bytes(&[b'x'; 7]), None).unwrap();
    }
    let mut listed = service.list("listing").unwrap();
    listed.sort();
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert!(listed.iter().all(|(_, size)| *size == 7), "{listed:?}");
    // The listed keys are blob keys: the service's own prefix is stripped back off.
    assert!(listed.iter().all(|(key, _)| !key.contains('/')), "{listed:?}");
}

/// The whole blob pipeline over a bucket: stage, analyze, and a tracked variant whose source has
/// to be pulled back out of the bucket for libvips. This is the flow the app runs for every image
/// attachment, and the one that was disk-only before.
#[test]
fn the_blob_pipeline_runs_over_a_bucket() {
    let Some(s3) = service("pipeline") else { return };
    let verifier = rails_compat::app_verifier(&rails_compat::Secrets::new("test"), "ActiveStorage");
    let storage = Storage::new(Service::S3(s3), verifier);
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("schema.sql")).unwrap();
    let now: jiff::Timestamp = "2026-10-01T12:00:00Z".parse().unwrap();

    let jpg = std::fs::read(fixture("moon.jpg")).expect("the reference submodule's fixtures");
    let mut blob = storage.create_and_upload(&conn, &jpg, Filename::new("moon.jpg"), None, now).unwrap();
    assert_eq!(storage.service.download(&blob.key).unwrap(), jpg);

    // `blob.analyze` reads the bytes back out of the bucket into a tempfile.
    storage.analyze(&conn, &mut blob).unwrap();
    assert_eq!(blob.metadata.get("analyzed"), Some(&campfire_storage::Json::Bool(true)));
    assert!(blob.metadata.get("width").is_some(), "{:?}", blob.metadata);

    // The variant: downloaded, transformed by libvips, uploaded under its own key.
    let variation = storage.variation_for(&blob, &campfire_storage::Variation::resize_to_limit(64, 64, None)).unwrap();
    let image = storage.process_variant(&conn, &blob, &variation, now).unwrap();
    assert!(storage.service.exist(&image.key), "the variant's bytes are in the bucket");
    assert_eq!(storage.service.stat(&image.key).unwrap().size, image.byte_size as u64);
    // Asked again, the variant record is reused and nothing is regenerated.
    assert_eq!(storage.process_variant(&conn, &blob, &variation, now).unwrap().id, image.id);

    // `Blob#delete` takes the object with it.
    let (original, variant) = (blob.key.clone(), image.key.clone());
    storage.delete_files(&blob).unwrap();
    storage.delete_files(&image).unwrap();
    assert!(!storage.service.exist(&original));
    assert!(!storage.service.exist(&variant));
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../reference/test/fixtures/files").join(name)
}
