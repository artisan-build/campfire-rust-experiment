//! The Active Storage endpoints (`activestorage/config/routes.rb`) over `campfire_storage`, as the
//! engine's controllers serve them, plus `ActiveStorage::Blob#purge` for the purge job.
//!
//! Downloads stay public behind signed URLs; the disk `PUT` and direct uploads require a
//! Campfire session (`reference/config/initializers/active_storage_authentication.rb`). The disk
//! service's `show` gets `Cache-Control: max-age=3600, public`
//! (`reference/config/initializers/active_storage.rb`). These controllers inherit from
//! `ActiveStorage::BaseController` (`protect_from_forgery with: :exception`), not
//! `ApplicationController`, so none of Campfire's concerns run.

use std::sync::{Arc, LazyLock};

use campfire_db::{CachedStatements, query_all};
use campfire_kit::{Ctx, Error, ExpiresIn, Freshness, Response, Result, SendOptions, StatusCode, halt, http::header};
use campfire_storage::file_server::{self, BodyPart};
use campfire_storage::{Blob, Filename, Json, Service, Source, Staged, Storage, Variation, content_types, disk, paths};
use rusqlite::{OptionalExtension, params};
use tokio::sync::Semaphore;

use crate::app::{App, AppCtx};
use crate::concerns::{find_session_by_cookie, head};

/// `ActiveStorage.service_urls_expire_in`
const SERVICE_URLS_EXPIRE_IN: i64 = 5 * 60;
/// `http_cache_forever`: `expires_in 100.years`.
const HUNDRED_YEARS: u64 = 3_155_695_200;
/// The most image and video jobs (variants, previews, analysis) that run at once.
const MAX_MEDIA_JOBS: usize = 4;

// --- Blobs -----------------------------------------------------------------------------------------

/// `ActiveStorage::Blobs::RedirectController#show`
pub async fn blobs_redirect(c: &mut Ctx) -> Result {
    c.verify_authenticity_token()?;
    let blob = set_blob(c).await?;
    c.expires_in(SERVICE_URLS_EXPIRE_IN as u64, ExpiresIn::default());
    let disposition = c.param_str("disposition").map(str::to_string);
    let url = blob_url(c, &blob, disposition.as_deref());
    c.redirect_to_with(&url, campfire_kit::Redirect { allow_other_host: true, ..Default::default() })
}

/// `ActiveStorage::Blobs::ProxyController#show`
pub async fn blobs_proxy(c: &mut Ctx) -> Result {
    c.verify_authenticity_token()?;
    let blob = set_blob(c).await?;
    let disposition = c.param_str("disposition").map(str::to_string);
    if let Some(range) = c.request.header("range").filter(|r| !r.trim().is_empty()).map(str::to_string) {
        return send_blob_byte_range_data(c, &blob, &range);
    }
    if let Some(not_modified) = http_cache_forever(c) {
        return Ok(not_modified);
    }
    let response = send_blob_stream(c, &blob, disposition.as_deref())?;
    Ok(response.header(header::ACCEPT_RANGES, "bytes"))
}

// --- Representations -------------------------------------------------------------------------------

/// `ActiveStorage::Representations::RedirectController#show`
pub async fn representations_redirect(c: &mut Ctx) -> Result {
    c.verify_authenticity_token()?;
    let blob = set_blob(c).await?;
    let image = set_representation(c, blob).await?;
    c.expires_in(SERVICE_URLS_EXPIRE_IN as u64, ExpiresIn::default());
    let disposition = c.param_str("disposition").map(str::to_string);
    let url = blob_url(c, &image, disposition.as_deref());
    c.redirect_to_with(&url, campfire_kit::Redirect { allow_other_host: true, ..Default::default() })
}

/// `ActiveStorage::Representations::ProxyController#show`
pub async fn representations_proxy(c: &mut Ctx) -> Result {
    c.verify_authenticity_token()?;
    let blob = set_blob(c).await?;
    let image = set_representation(c, blob).await?;
    if let Some(not_modified) = http_cache_forever(c) {
        return Ok(not_modified);
    }
    let disposition = c.param_str("disposition").map(str::to_string);
    send_blob_stream(c, &image, disposition.as_deref())
}

/// `ActiveStorage::SetBlob#set_blob`: `Blob.find_signed!(params[:signed_blob_id] || params[:signed_id])`.
/// A bad signature is `head :not_found`; a valid one for a missing blob is `RecordNotFound`.
async fn set_blob(c: &mut Ctx) -> Result<Blob> {
    let signed_id = c.param_str("signed_blob_id").or_else(|| c.param_str("signed_id")).unwrap_or("").to_string();
    let storage = c.app().storage.clone();
    let Some(blob_id) = paths::verify_signed_blob_id(&storage.verifier, &signed_id, c.now()) else {
        return halt(head(StatusCode::NOT_FOUND));
    };
    c.app().read(move |conn| Blob::find(conn, blob_id).map_err(storage_error)).await?.ok_or(Error::NotFound)
}

/// `set_representation`: `@blob.representation(params[:variation_key]).processed`. A bad
/// variation key is `head :not_found`. Returns the blob that represents it (the variant's or
/// preview's image).
async fn set_representation(c: &mut Ctx, blob: Blob) -> Result<Blob> {
    let storage = c.app().storage.clone();
    let key = c.param_str("variation_key").unwrap_or("").to_string();
    let variation = match Variation::decode(&storage.verifier, &key, c.now()) {
        Ok(variation) => variation,
        Err(campfire_storage::Error::InvalidSignature) => return halt(head(StatusCode::NOT_FOUND)),
        Err(error) => return Err(Error::internal(error)),
    };
    processed_representation(c.app(), blob, variation).await
}

/// `blob.representation(variation).processed`, reusing an existing variant or preview.
pub async fn processed_representation(app: &App, blob: Blob, variation: Variation) -> Result<Blob> {
    if blob.is_previewable() {
        processed_preview(app, blob, variation).await
    } else if blob.is_variable() {
        let variation = app.storage.variation_for(&blob, &variation).map_err(Error::internal)?;
        processed_variant(app, blob, variation).await
    } else {
        Err(Error::internal(campfire_storage::Error::Unrepresentable(blob.content_type().to_string())))
    }
}

/// `blob.preview(transformations).processed`: the preview image itself for empty
/// transformations, otherwise its processed variant.
pub async fn processed_preview(app: &App, blob: Blob, transformations: Variation) -> Result<Blob> {
    let image = preview_image(app, blob).await?;
    if transformations.is_empty() {
        return Ok(image);
    }
    let variation = app.storage.variation_for(&image, &transformations).map_err(Error::internal)?;
    processed_variant(app, image, variation).await
}

/// `VariantWithRecord#processed` for an already-defaulted variation: the existing variant, or
/// one transformed off the writer and then recorded.
async fn processed_variant(app: &App, blob: Blob, variation: Variation) -> Result<Blob> {
    processed_variant_with(app, blob, variation, |storage, blob, variation| storage.transform_variant(blob, variation)).await
}

pub(crate) async fn processed_variant_with(
    app: &App,
    blob: Blob,
    variation: Variation,
    transform: impl FnOnce(&Storage, &Blob, &Variation) -> campfire_storage::Result<Staged> + Send + 'static,
) -> Result<Blob> {
    let storage = app.storage.clone();
    let (source, digested) = (blob.clone(), variation.clone());
    let existing = app.read(move |conn| storage.existing_variant(conn, &source, &digested).map_err(storage_error)).await?;
    if let Some(image) = existing {
        return Ok(image);
    }

    let storage = app.storage.clone();
    let (source, digested) = (blob.clone(), variation.clone());
    let image = process_media(move || transform(&storage, &source, &digested)).await?;

    let storage = app.storage.clone();
    app.write(move |tx| {
        let conn = tx.conn();
        match storage.record_variant(conn, &blob, &variation, &image, tx.now().jiff()).map_err(storage_error)? {
            Some(recorded) => {
                keep_after_commit(tx, image);
                Ok(recorded)
            }
            // Another request recorded it first; ours is dropped (and its file deleted).
            None => storage
                .existing_variant(conn, &blob, &variation)
                .map_err(storage_error)?
                .ok_or(campfire_db::Error::RecordNotFound("ActiveStorage::VariantRecord")),
        }
    })
    .await
}

/// `blob.preview_image`, drawing it with ffmpeg off the writer when it's missing.
async fn preview_image(app: &App, blob: Blob) -> Result<Blob> {
    let storage = app.storage.clone();
    let source = blob.clone();
    let existing = app.read(move |conn| storage.existing_preview_image(conn, &source).map_err(storage_error)).await?;
    if let Some(image) = existing {
        return Ok(image);
    }

    let storage = app.storage.clone();
    let source = blob.clone();
    let image = process_media(move || storage.draw_preview_image(&source)).await?;

    let storage = app.storage.clone();
    app.write(move |tx| {
        let conn = tx.conn();
        match storage.record_preview_image(conn, &blob, &image, tx.now().jiff()).map_err(storage_error)? {
            Some(recorded) => {
                keep_after_commit(tx, image);
                Ok(recorded)
            }
            None => storage
                .existing_preview_image(conn, &blob)
                .map_err(storage_error)?
                .ok_or(campfire_db::Error::RecordNotFound("ActiveStorage::Blob")),
        }
    })
    .await
}

/// What `blob.analyze` would save, worked out off the writer.
pub async fn analyzed_metadata(app: &App, blob: &Blob) -> Result<Json> {
    let (storage, blob) = (app.storage.clone(), blob.clone());
    process_media(move || storage.analyzed_metadata(&blob)).await
}

/// Uploads a file to storage for a blob whose row the caller saves next (see [`keep_after_commit`]).
pub async fn stage_file(app: &App, path: std::path::PathBuf, filename: Filename, content_type: Option<String>) -> Result<Staged> {
    let storage = app.storage.clone();
    tokio::task::spawn_blocking(move || storage.stage_file(&path, filename, content_type.as_deref()))
        .await
        .map_err(Error::internal)?
        .map_err(Error::internal)
}

/// Keeps a staged file once the write saving its row commits; a rollback drops it instead,
/// which deletes the file.
pub fn keep_after_commit(tx: &mut campfire_db::Tx<'_>, staged: Staged) {
    tx.after_commit(move |_| {
        staged.keep();
        Ok(())
    });
}

/// Runs libvips, ffmpeg or ffprobe work on the blocking pool, a few jobs at a time: each can take
/// a lot of memory and CPU (libvips threads its own work), and uploads shouldn't queue behind
/// more of them than the machine can run at once.
async fn process_media<T: Send + 'static>(work: impl FnOnce() -> campfire_storage::Result<T> + Send + 'static) -> Result<T> {
    static PERMITS: LazyLock<Arc<Semaphore>> =
        LazyLock::new(|| Arc::new(Semaphore::new(std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, MAX_MEDIA_JOBS))));
    // The permit goes with the work: a request that gives up (a timeout, a closed connection)
    // doesn't stop the blocking task, so it mustn't free the slot either.
    let permit = PERMITS.clone().acquire_owned().await.map_err(Error::internal)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(Error::internal)?
    .map_err(Error::internal)
}

/// `blob.url(disposition:)` on the disk service: a signed `/rails/active_storage/disk/...` URL
/// on this request's host that expires in `service_urls_expire_in`.
fn blob_url(c: &Ctx, blob: &Blob, disposition: Option<&str>) -> String {
    let storage = &c.app().storage;
    let content_type = content_types::for_serving(blob.content_type());
    let disposition = content_types::forced_disposition(blob.content_type()).or(disposition).unwrap_or("inline");
    let expires_at = c.now() + jiff::SignedDuration::from_secs(SERVICE_URLS_EXPIRE_IN);
    let path = storage.service.url_path(&storage.verifier, &blob.key, Some(expires_at), &blob.filename, Some(content_type), disposition);
    c.url_for(&path)
}

/// `http_cache_forever(public: true)`: cache for 100 years, ETag on the full path, and a fixed
/// Last-Modified. `Some(304)` when the client's copy is fresh.
fn http_cache_forever(c: &mut Ctx) -> Option<Response> {
    c.expires_in(HUNDRED_YEARS, ExpiresIn { public: true, immutable: true, ..ExpiresIn::default() });
    let last_modified: jiff::Timestamp = "2011-01-01T00:00:00Z".parse().expect("valid timestamp");
    c.fresh_when(Freshness { etag: Some(c.request.fullpath()), last_modified: Some(last_modified), public: true, ..Freshness::default() })
}

/// `send_blob_stream(blob, disposition:)`: the whole object, inline unless the type is forced to
/// download.
fn send_blob_stream(c: &mut Ctx, blob: &Blob, disposition: Option<&str>) -> Result {
    let storage = c.app().storage.clone();
    let stat = match storage.service.stat(&blob.key) {
        Ok(stat) => stat,
        Err(campfire_storage::Error::FileNotFound) => {
            // `rescue ActiveStorage::FileNotFoundError`: expires_now, head :not_found.
            c.expires_now();
            return Ok(c.head(StatusCode::NOT_FOUND));
        }
        Err(error) => return Err(Error::internal(error)),
    };
    let disposition = content_types::forced_disposition(blob.content_type()).or(disposition).unwrap_or("inline");
    let content_type = content_types::for_serving(blob.content_type()).to_string();
    let parts = vec![BodyPart::Range { start: 0, end: stat.size.saturating_sub(1) }];
    let response = send_object(c, &storage.service, &blob.key, content_type, stat.size, parts);
    Ok(with_disposition(response, disposition, blob))
}

/// `send_file service.path_for(variant.key), content_type:, disposition: :inline` as the avatar
/// and logo controllers used to do it. They are the one place outside this module that sends a
/// blob's bytes directly, and only the disk service has a path to send, so the response is built
/// the same way as every other object response here. The `Content-Disposition` still carries the
/// key as its filename, which is what `send_file` derived from the path.
pub fn send_variant(c: &mut Ctx, variant: &Blob, content_type: &str) -> Result {
    let storage = c.app().storage.clone();
    let stat = storage.service.stat(&variant.key).map_err(Error::internal)?;
    let parts = vec![BodyPart::Range { start: 0, end: stat.size.saturating_sub(1) }];
    let mut response =
        c.send_data(bytes::Bytes::new(), SendOptions { filename: Some(variant.key.clone()), ..SendOptions::inline(content_type) });
    response.body = object_body(&storage.service, &variant.key, parts);
    if matches!(response.body, campfire_kit::Body::Stream(_)) {
        response = response.header(header::CONTENT_LENGTH, &stat.size.to_string());
    }
    Ok(response)
}

/// The `send_data`/`send_file` response for `len` bytes of an object, with `parts` as its body.
/// `send_file` can't be used directly any more: only the disk service has a path, and the body
/// machinery below is what makes the same response work for an object in a bucket.
fn send_object(c: &mut Ctx, service: &Service, key: &str, content_type: String, len: u64, parts: Vec<BodyPart>) -> Response {
    let mut response =
        c.send_data(bytes::Bytes::new(), SendOptions { content_type: Some(content_type), disposition: None, ..SendOptions::default() });
    response.body = object_body(service, key, parts);
    // A streamed body has no length of its own; a file body gets one from the kit.
    if matches!(response.body, campfire_kit::Body::Stream(_)) {
        response = response.header(header::CONTENT_LENGTH, &len.to_string());
    }
    response
}

/// `send_data`/`send_stream`'s `Content-Disposition` for the blob's sanitized filename.
fn with_disposition(response: Response, disposition: &str, blob: &Blob) -> Response {
    response.header(header::CONTENT_DISPOSITION, &rails_compat::content_disposition::format(disposition, &blob.filename.sanitized()))
}

/// `send_blob_byte_range_data(blob, range_header)`
fn send_blob_byte_range_data(c: &mut Ctx, blob: &Blob, range: &str) -> Result {
    let storage = c.app().storage.clone();
    let size = blob.byte_size.max(0) as u64;
    let ranges = match ruby_compat::rack::byte_ranges(Some(range), size) {
        Some(ranges) if !ranges.is_empty() => ranges,
        _ => return Ok(c.head(StatusCode::RANGE_NOT_SATISFIABLE)),
    };
    if !storage.service.exist(&blob.key) {
        return Err(Error::internal(campfire_storage::Error::FileNotFound));
    }
    let content_type_for_serving = content_types::for_serving(blob.content_type()).to_string();
    let (content_type, parts, content_range) = if let [(start, end)] = ranges[..] {
        (content_type_for_serving, vec![BodyPart::Range { start, end }], Some(format!("bytes {start}-{end}/{size}")))
    } else {
        // `SecureRandom.hex`: 16 random bytes.
        let boundary = hex::encode(rand::random::<[u8; 16]>());
        let mut parts = Vec::new();
        for &(start, end) in &ranges {
            let heading = format!(
                "\r\n--{boundary}\r\nContent-Type: {content_type_for_serving}\r\nContent-Range: bytes {start}-{end}/{size}\r\n\r\n"
            );
            parts.push(BodyPart::Bytes(heading.into_bytes()));
            parts.push(BodyPart::Range { start, end });
        }
        parts.push(BodyPart::Bytes(format!("\r\n--{boundary}--\r\n").into_bytes()));
        (format!("multipart/byteranges; boundary={boundary}"), parts, None)
    };
    let disposition = content_types::forced_disposition(blob.content_type()).unwrap_or("inline");
    let response = c.send_data(
        bytes::Bytes::new(),
        SendOptions { content_type: Some(content_type), disposition: None, status: StatusCode::PARTIAL_CONTENT, ..SendOptions::default() },
    );
    let mut response = with_disposition(response, disposition, blob);
    let length = parts_len(&parts);
    response.body = object_body(&storage.service, &blob.key, parts);
    if matches!(response.body, campfire_kit::Body::Stream(_)) {
        response = response.header(header::CONTENT_LENGTH, &length.to_string());
    }
    if let Some(content_range) = content_range {
        response = response.header(header::CONTENT_RANGE, &content_range);
    }
    Ok(response.header(header::ACCEPT_RANGES, "bytes"))
}

/// The body for byte ranges of an object and the bytes between them. One whole-file range on the
/// disk service stays a file body, which the kit streams straight off the filesystem; everything
/// else is read from the service a part at a time, so nothing is buffered up front — a blob in a
/// bucket is a ranged `GET`, not a download into memory.
fn object_body(service: &Service, key: &str, parts: Vec<BodyPart>) -> campfire_kit::Body {
    match <[BodyPart; 1]>::try_from(parts) {
        Ok([BodyPart::Range { start, end }]) if service.local_path(key).is_some() => {
            let path = service.local_path(key).expect("matched above");
            campfire_kit::Body::File(campfire_kit::response::FileBody { path, offset: start, len: end - start + 1 })
        }
        Ok([BodyPart::Bytes(bytes)]) => campfire_kit::Body::Bytes(bytes.into()),
        Ok([part]) => stream_body(service, key, vec![part]),
        Err(parts) if parts.is_empty() => campfire_kit::Body::Empty,
        Err(parts) => stream_body(service, key, parts),
    }
}

fn parts_len(parts: &[BodyPart]) -> u64 {
    parts
        .iter()
        .map(|part| match part {
            BodyPart::Bytes(bytes) => bytes.len() as u64,
            BodyPart::Range { start, end } => end - start + 1,
        })
        .sum()
}

/// Reads each part in turn, a chunk at a time, on a blocking thread: every service read is
/// blocking (the bucket client is a blocking HTTP client, see `campfire_storage::s3`), so the
/// reading cannot happen on a runtime worker. The channel gives back-pressure, and dropping the
/// response drops the receiver, which ends the task on its next send.
fn stream_body(service: &Service, key: &str, parts: Vec<BodyPart>) -> campfire_kit::Body {
    use std::io::Read;
    let (service, key) = (service.clone(), key.to_string());
    const CHUNK: usize = 64 * 1024;
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(2);
    tokio::task::spawn_blocking(move || {
        for part in parts {
            let mut reader: Box<dyn Read + Send> = match part {
                BodyPart::Bytes(bytes) => {
                    if tx.blocking_send(Ok(bytes::Bytes::from(bytes))).is_err() {
                        return;
                    }
                    continue;
                }
                BodyPart::Range { start, end } => match service.open_range(&key, start, end - start + 1) {
                    Ok(reader) => reader,
                    Err(error) => {
                        let _ = tx.blocking_send(Err(std::io::Error::other(error)));
                        return;
                    }
                },
            };
            loop {
                let mut chunk = vec![0u8; CHUNK];
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        chunk.truncate(read);
                        if tx.blocking_send(Ok(bytes::Bytes::from(chunk))).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx.blocking_send(Err(error));
                        return;
                    }
                }
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    campfire_kit::Body::Stream(axum::body::Body::from_stream(stream))
}

// --- Disk service ----------------------------------------------------------------------------------

/// `ActiveStorage::DiskController#show`, plus the initializer's `after_action` cache header.
pub async fn disk_show(c: &mut Ctx) -> Result {
    let response = disk_serve(c)?;
    Ok(response.header(header::CACHE_CONTROL, "max-age=3600, public"))
}

fn disk_serve(c: &mut Ctx) -> Result {
    let storage = c.app().storage.clone();
    let encoded_key = c.param_str("encoded_key").unwrap_or("").to_string();
    let Some(key) = disk::decode_verified_key(&storage.verifier, &encoded_key, c.now()) else {
        return Ok(c.head(StatusCode::NOT_FOUND));
    };
    let request = file_server::Request {
        method: c.request.method.as_str(),
        range: c.request.header("range"),
        if_modified_since: c.request.header("if-modified-since"),
    };
    let stat = match storage.service.stat(&key.key) {
        Ok(stat) => stat,
        Err(campfire_storage::Error::FileNotFound) => return Ok(c.head(StatusCode::NOT_FOUND)),
        Err(error) => return Err(Error::internal(error)),
    };
    let served = file_server::serve_file(&request, &stat, key.content_type.as_deref(), Some(&key.disposition));
    let mut response = Response::new(StatusCode::from_u16(served.status).map_err(Error::internal)?);
    for (name, value) in &served.headers {
        response = response.header(name.as_str(), value);
    }
    // `served.headers` carries the Content-Length of every part together.
    response.body = object_body(&storage.service, &key.key, served.body);
    Ok(response)
}

/// `ActiveStorage::DiskController#update` (the direct-upload PUT), behind
/// `require_active_storage_authentication`.
pub async fn disk_update(c: &mut Ctx) -> Result {
    require_active_storage_authentication(c).await?;
    let storage = c.app().storage.clone();
    let encoded_token = c.param_str("encoded_token").unwrap_or("").to_string();
    let Some(token) = disk::decode_verified_token(&storage.verifier, &encoded_token, c.now()) else {
        return Ok(c.head(StatusCode::NOT_FOUND));
    };
    if !acceptable_content(c, &token) {
        return Ok(c.head(StatusCode::UNPROCESSABLE_ENTITY));
    }
    let body = c.request.raw_post().clone();
    let (key, checksum) = (token.key.clone(), token.checksum.clone());
    let uploaded = tokio::task::spawn_blocking(move || storage.service.upload(&key, Source::Bytes(body.as_ref()), Some(&checksum)))
        .await
        .map_err(Error::internal)?;
    match uploaded {
        Ok(()) => Ok(c.head(StatusCode::NO_CONTENT)),
        Err(campfire_storage::Error::Integrity) => Ok(c.head(StatusCode::UNPROCESSABLE_ENTITY)),
        Err(error) => Err(Error::internal(error)),
    }
}

/// `token[:content_type] == request.content_mime_type && token[:content_length] == request.content_length`
fn acceptable_content(c: &Ctx, token: &disk::DiskToken) -> bool {
    let media_type = c.request.media_type();
    let content_length = c.request.header("content-length").and_then(|l| l.trim().parse::<i64>().ok());
    token.content_type.as_deref().map(str::to_ascii_lowercase) == media_type.map(|m| m.to_ascii_lowercase())
        && Some(token.content_length) == content_length
}

/// `ActiveStorage::DirectUploadsController#create`, behind CSRF and
/// `require_active_storage_authentication`.
pub async fn direct_uploads_create(c: &mut Ctx) -> Result {
    c.verify_authenticity_token()?;
    require_active_storage_authentication(c).await?;
    // `params.expect(blob: [:filename, :byte_size, :checksum, :content_type, metadata: {}])`
    let blob_params = c.params.require("blob")?.as_hash().cloned().ok_or_else(|| Error::ParameterMissing("blob".into()))?;
    // Strings, and numbers as their text: Active Storage's JavaScript sends `byte_size` as a number.
    let text = |key: &str| {
        blob_params.get(key).and_then(|p| match p {
            campfire_kit::Param::Str(s) => Some(s.clone()),
            campfire_kit::Param::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };
    let (Some(filename), Some(checksum)) = (text("filename").filter(|f| !f.is_empty()), text("checksum").filter(|c| !c.is_empty())) else {
        return Err(Error::Status(StatusCode::UNPROCESSABLE_ENTITY));
    };
    // Stricter than Rails, whose attribute cast makes a byte size that isn't a number 0 (an
    // upload only an empty file could fill): it's refused.
    let Some(byte_size) = text("byte_size").and_then(|s| ruby_compat::integer_cast(&s)) else {
        return Err(Error::Status(StatusCode::UNPROCESSABLE_ENTITY));
    };
    // The upload's PUT body is read into memory, so it's capped like other bodies: don't hand out
    // a URL for more than it will accept. (Campfire's editor only attaches mentions and embeds;
    // files go up with the message form.)
    if !(0..=campfire_kit::body::MAX_BUFFERED_BODY as i64).contains(&byte_size) {
        return Err(Error::Status(StatusCode::PAYLOAD_TOO_LARGE));
    }
    let content_type = text("content_type");
    let metadata = match blob_params.get("metadata").and_then(|m| m.as_hash()) {
        Some(metadata) => Json::parse(&metadata.to_json().to_string()).map_err(Error::internal)?,
        None => Json::object(),
    };

    let storage = c.app().storage.clone();
    let now = c.now();
    let new_blob = campfire_storage::NewBlob {
        key: campfire_storage::key::generate_key(),
        filename: Filename::new(filename),
        content_type: content_type.clone(),
        metadata,
        service_name: storage.service.name().to_string(),
        byte_size,
        checksum: checksum.clone(),
    };
    let blob = c.app().write(move |tx| new_blob.insert(tx.conn(), now).map_err(storage_error)).await?;

    let expires_at = now + jiff::SignedDuration::from_secs(SERVICE_URLS_EXPIRE_IN);
    let url = c.url_for(&storage.service.url_path_for_direct_upload(
        &storage.verifier,
        &blob.key,
        expires_at,
        content_type.as_deref(),
        byte_size,
        &checksum,
    ));
    let signed_id = paths::signed_blob_id(&storage.verifier, blob.id, None);
    let json = direct_upload_json(&blob, &signed_id, &url, content_type.as_deref());
    Ok(c.render_as(StatusCode::OK, campfire_kit::response::JSON_UTF8, json))
}

/// `blob.as_json(root: false, methods: :signed_id).merge(direct_upload: { url:, headers: })`
fn direct_upload_json(blob: &Blob, signed_id: &str, url: &str, content_type: Option<&str>) -> String {
    let text = |s: Option<&str>| s.map_or(Json::Null, Json::from);
    Json::Object(vec![
        ("id".into(), Json::Int(blob.id)),
        ("key".into(), blob.key.as_str().into()),
        ("filename".into(), blob.filename.raw().into()),
        ("content_type".into(), text(blob.content_type.as_deref())),
        ("metadata".into(), blob.metadata.clone()),
        ("service_name".into(), blob.service_name.as_str().into()),
        ("byte_size".into(), Json::Int(blob.byte_size)),
        ("checksum".into(), text(blob.checksum.as_deref())),
        ("created_at".into(), Json::String(json_time(&blob.created_at))),
        ("signed_id".into(), signed_id.into()),
        (
            "direct_upload".into(),
            Json::Object(vec![
                ("url".into(), url.into()),
                ("headers".into(), Json::Object(vec![("Content-Type".into(), text(content_type))])),
            ]),
        ),
    ])
    .encode()
}

/// A stored `created_at` (`YYYY-MM-DD HH:MM:SS[.ffffff]`, UTC) as `ActiveSupport::JSON` encodes
/// times: ISO 8601 with milliseconds.
fn json_time(db_time: &str) -> String {
    let parsed = jiff::civil::DateTime::strptime("%Y-%m-%d %H:%M:%S%.f", db_time)
        .or_else(|_| jiff::civil::DateTime::strptime("%Y-%m-%d %H:%M:%S", db_time));
    match parsed {
        Ok(time) => time.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        Err(_) => db_time.to_string(),
    }
}

/// `ActiveStorageAuthentication#require_active_storage_authentication`: 401 without a session.
async fn require_active_storage_authentication(c: &mut Ctx) -> Result<()> {
    if find_session_by_cookie(c).await?.is_none() {
        return halt(head(StatusCode::UNAUTHORIZED));
    }
    Ok(())
}

// --- Purging ---------------------------------------------------------------------------------------

/// `ActiveStorage::Blob#purge`: `destroy` (refused while any attachment still points at the
/// blob; destroys its variant records and preview image attachment, whose blobs are purged
/// later), then delete the files.
pub async fn purge(app: &App, blob_id: i64) -> anyhow::Result<()> {
    let destroyed = app
        .db
        .write(move |tx| {
            let conn = tx.conn();
            let Some(blob) = Blob::find(conn, blob_id).map_err(storage_error)? else { return Ok(None) };
            // before_destroy(prepend: true) { raise ActiveRecord::InvalidForeignKey if attachments.exists? }
            if !campfire_storage::blob::attachment_records(conn, blob_id).map_err(storage_error)?.is_empty() {
                return Ok(None);
            }
            let mut dependents = Vec::new();
            // before_destroy { variant_records.destroy_all }: each record's image attachment goes too.
            let variant_records: Vec<i64> =
                query_all(conn, "SELECT id FROM active_storage_variant_records WHERE blob_id = ?1", [blob_id], |row| row.get(0))?;
            for record_id in variant_records {
                dependents.extend(destroy_attachment(conn, "ActiveStorage::VariantRecord", record_id, "image")?);
                conn.execute_cached("DELETE FROM active_storage_variant_records WHERE id = ?1", [record_id])?;
            }
            // has_one_attached :preview_image (dependent: :destroy on the attachment)
            dependents.extend(destroy_attachment(conn, "ActiveStorage::Blob", blob_id, "preview_image")?);
            conn.execute_cached("DELETE FROM active_storage_blobs WHERE id = ?1", [blob_id])?;
            // after_destroy_commit :purge_dependent_blob_later
            for dependent in &dependents {
                tx.emit_after_commit(campfire_db::Event::PurgeBlob { blob_id: *dependent });
            }
            Ok(Some(blob))
        })
        .await?;
    if let Some(blob) = destroyed {
        delete_files(app.storage.clone(), blob).await?;
    }
    Ok(())
}

async fn delete_files(storage: Arc<Storage>, blob: Blob) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || storage.delete_files(&blob)).await??;
    Ok(())
}

/// Deletes the attachment row, returning its blob id.
fn destroy_attachment(conn: &rusqlite::Connection, record_type: &str, record_id: i64, name: &str) -> campfire_db::Result<Option<i64>> {
    let attachment: Option<(i64, i64)> = conn
        .query_row_cached(
            "SELECT id, blob_id FROM active_storage_attachments WHERE record_type = ?1 AND record_id = ?2 AND name = ?3 LIMIT 1",
            params![record_type, record_id, name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((id, blob_id)) = attachment else { return Ok(None) };
    conn.execute_cached("DELETE FROM active_storage_attachments WHERE id = ?1", [id])?;
    Ok(Some(blob_id))
}

/// A storage error inside a database closure. SQLite's own errors stay `Error::Sqlite`, so that
/// `is_record_not_unique` sees them. (Neither crate depends on the other, so this can't be a
/// `From` impl.)
pub fn storage_error(error: campfire_storage::Error) -> campfire_db::Error {
    match error {
        campfire_storage::Error::Sql(error) => error.into(),
        other => campfire_db::Error::other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique index refusing a blob or attachment row inside `User.create!` is rescued as
    /// `ActiveRecord::RecordNotUnique`, like the user row's own.
    #[test]
    fn storage_errors_keep_sqlites_own() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE blobs (key TEXT UNIQUE); INSERT INTO blobs VALUES ('a')").unwrap();
        let unique = conn.execute("INSERT INTO blobs VALUES ('a')", []).unwrap_err();
        assert!(storage_error(campfire_storage::Error::Sql(unique)).is_record_not_unique());
        assert!(matches!(storage_error(campfire_storage::Error::FileNotFound), campfire_db::Error::Other(_)));
    }

    /// The disk service and a bucket are read by the same code path; what differs is only that
    /// one has a local file and the other does not, so both are driven here over a disk service
    /// (whose `local_path` can be suppressed by asking for several parts).
    #[tokio::test]
    async fn byte_ranges_are_not_read_into_memory() {
        let root = tempfile::tempdir().unwrap();
        let contents: Vec<u8> = (0..=255u8).cycle().take(200_000).collect();
        let service = Service::disk(root.path(), "local");
        let key = "abcdefghijklmnopqrstuvwxyz12";
        service.upload(key, Source::Bytes(&contents), None).unwrap();
        let range = |start, end| BodyPart::Range { start, end };

        match object_body(&service, key, vec![range(10, 199_999)]) {
            campfire_kit::Body::File(body) => assert_eq!((body.offset, body.len), (10, 199_990)),
            other => panic!("a single range on disk should be a file body, got {other:?}"),
        }

        let parts = vec![BodyPart::Bytes(b"<".to_vec()), range(0, 2), BodyPart::Bytes(b">".to_vec()), range(100_000, 170_000)];
        let length = parts_len(&parts);
        let campfire_kit::Body::Stream(stream) = object_body(&service, key, parts) else { panic!("several ranges should stream") };
        let streamed = axum::body::to_bytes(stream, usize::MAX).await.unwrap();
        assert_eq!(streamed, [b"<".as_slice(), &contents[0..3], b">", &contents[100_000..=170_000]].concat());
        assert_eq!(streamed.len() as u64, length);
    }

    /// A service with no local file (a bucket) streams even a single whole range, and the bytes
    /// are the object's. `Service::S3` can't be reached from a unit test, so the streaming half
    /// of `object_body` is driven directly.
    #[tokio::test]
    async fn a_service_without_a_local_file_streams_the_object() {
        let root = tempfile::tempdir().unwrap();
        let service = Service::disk(root.path(), "local");
        let key = "abcdefghijklmnopqrstuvwxyz12";
        service.upload(key, Source::Bytes(b"0123456789"), None).unwrap();
        let body = stream_body(&service, key, vec![BodyPart::Range { start: 2, end: 5 }]);
        let campfire_kit::Body::Stream(stream) = body else { panic!("a streamed body") };
        assert_eq!(axum::body::to_bytes(stream, usize::MAX).await.unwrap(), b"2345".as_slice());
    }

    /// A read that fails mid-body ends the stream with an error rather than silently truncating.
    #[tokio::test]
    async fn a_missing_object_ends_the_stream_with_an_error() {
        let root = tempfile::tempdir().unwrap();
        let service = Service::disk(root.path(), "local");
        let body = stream_body(&service, "absentkeyabsentkeyabsentkey1", vec![BodyPart::Range { start: 0, end: 9 }]);
        let campfire_kit::Body::Stream(stream) = body else { panic!("a streamed body") };
        assert!(axum::body::to_bytes(stream, usize::MAX).await.is_err());
    }

    #[test]
    fn json_times_have_milliseconds() {
        assert_eq!(json_time("2026-03-02 16:00:00.123456"), "2026-03-02T16:00:00.123Z");
        assert_eq!(json_time("2026-03-02 16:00:00"), "2026-03-02T16:00:00.000Z");
    }
}
