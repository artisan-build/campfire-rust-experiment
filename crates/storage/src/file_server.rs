//! `ActiveStorage::FileServer#serve_file` on top of `Rack::Files#serving` (rack 3.2): conditional
//! GET on mtime, single and multipart byte ranges, and 416s. HTTP-framework *and* storage-service
//! agnostic: it is given the object's [`Stat`] and returns the response plus the byte ranges of
//! the object to send, which the caller reads from whichever service holds it.

use ruby_compat::rack::byte_ranges;

use crate::service::Stat;

pub const MULTIPART_BOUNDARY: &str = "AaB03x";

#[derive(Debug, PartialEq)]
pub struct Served {
    pub status: u16,
    /// In Rack's order; `content-type` and `content-disposition` are set last by FileServer.
    pub headers: Vec<(String, String)>,
    pub body: Vec<BodyPart>,
}

#[derive(Debug, PartialEq)]
pub enum BodyPart {
    Bytes(Vec<u8>),
    /// An inclusive byte range of the object being served.
    Range {
        start: u64,
        end: u64,
    },
}

pub struct Request<'a> {
    pub method: &'a str,
    pub range: Option<&'a str>,
    pub if_modified_since: Option<&'a str>,
}

/// `serve_file(path, content_type:, disposition:)` as `DiskController#show` calls it.
pub fn serve_file(request: &Request, stat: &Stat, content_type: Option<&str>, disposition: Option<&str>) -> Served {
    let mut served = serving(request, stat);
    if served.status == 416 {
        served.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("x-cascade"));
    }
    set_header(&mut served.headers, "content-type", content_type.unwrap_or("application/octet-stream"));
    set_header(&mut served.headers, "content-disposition", disposition.unwrap_or("attachment"));
    served
}

fn serving(request: &Request, stat: &Stat) -> Served {
    if request.method == "OPTIONS" {
        return Served {
            status: 200,
            headers: vec![("allow".into(), "GET, HEAD, OPTIONS".into()), ("content-length".into(), "0".into())],
            body: vec![],
        };
    }
    let last_modified = httpdate(stat.modified);
    if request.if_modified_since == Some(last_modified.as_str()) {
        return Served { status: 304, headers: vec![], body: vec![] };
    }

    // Disk keys have no extension, so Rack's mime lookup falls back to its default.
    let mime_type = "text/plain";
    let mut headers = vec![("last-modified".to_string(), last_modified), ("content-type".to_string(), mime_type.to_string())];
    let size = stat.size;

    let (status, body, length) = match byte_ranges(request.range, size) {
        None => (200, vec![BodyPart::Range { start: 0, end: size.saturating_sub(1) }], size),
        Some(ranges) if ranges.is_empty() => {
            let body = "Byte range unsatisfiable\n";
            return Served {
                status: 416,
                headers: vec![
                    ("content-type".into(), "text/plain".into()),
                    ("content-length".into(), body.len().to_string()),
                    ("x-cascade".into(), "pass".into()),
                    ("content-range".into(), format!("bytes */{size}")),
                ],
                body: vec![BodyPart::Bytes(body.as_bytes().to_vec())],
            };
        }
        Some(ranges) => {
            let mut parts = Vec::new();
            if ranges.len() == 1 {
                let (start, end) = ranges[0];
                headers.push(("content-range".into(), format!("bytes {start}-{end}/{size}")));
                parts.push(BodyPart::Range { start, end });
            } else {
                set_header(&mut headers, "content-type", &format!("multipart/byteranges; boundary={MULTIPART_BOUNDARY}"));
                for &(start, end) in &ranges {
                    let heading = format!(
                        "\r\n--{MULTIPART_BOUNDARY}\r\ncontent-type: {mime_type}\r\ncontent-range: bytes {start}-{end}/{size}\r\n\r\n"
                    );
                    parts.push(BodyPart::Bytes(heading.into_bytes()));
                    parts.push(BodyPart::Range { start, end });
                }
                parts.push(BodyPart::Bytes(format!("\r\n--{MULTIPART_BOUNDARY}--\r\n").into_bytes()));
            }
            let length = parts
                .iter()
                .map(|part| match part {
                    BodyPart::Bytes(bytes) => bytes.len() as u64,
                    BodyPart::Range { start, end } => end - start + 1,
                })
                .sum();
            (206, parts, length)
        }
    };

    headers.push(("content-length".into(), length.to_string()));
    let body = if request.method == "HEAD" || size == 0 { vec![] } else { body };
    Served { status, headers, body }
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    match headers.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        Some(entry) => entry.1 = value.to_string(),
        None => headers.push((name.to_string(), value.to_string())),
    }
}

/// `Time#httpdate`: `Sun, 06 Nov 1994 08:49:37 GMT`.
fn httpdate(time: jiff::Timestamp) -> String {
    time.strftime("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: Stat = Stat { size: 10, modified: jiff::Timestamp::UNIX_EPOCH };

    fn get(range: Option<&str>) -> Request<'_> {
        Request { method: "GET", range, if_modified_since: None }
    }

    #[test]
    fn the_whole_object_is_one_range_over_its_size() {
        let served = serve_file(&get(None), &STAT, Some("image/png"), Some("inline"));
        assert_eq!(served.status, 200);
        assert_eq!(served.body, vec![BodyPart::Range { start: 0, end: 9 }]);
        assert!(served.headers.contains(&("content-length".into(), "10".into())));
        assert!(served.headers.contains(&("last-modified".into(), "Thu, 01 Jan 1970 00:00:00 GMT".into())));
        assert!(served.headers.contains(&("content-type".into(), "image/png".into())));
    }

    #[test]
    fn a_single_range_is_a_206_over_just_those_bytes() {
        let served = serve_file(&get(Some("bytes=2-4")), &STAT, None, None);
        assert_eq!(served.status, 206);
        assert_eq!(served.body, vec![BodyPart::Range { start: 2, end: 4 }]);
        assert!(served.headers.contains(&("content-range".into(), "bytes 2-4/10".into())));
        assert!(served.headers.contains(&("content-length".into(), "3".into())));
    }

    #[test]
    fn an_unsatisfiable_range_is_a_416_with_no_x_cascade() {
        let served = serve_file(&get(Some("bytes=20-30")), &STAT, None, None);
        assert_eq!(served.status, 416);
        assert!(!served.headers.iter().any(|(name, _)| name == "x-cascade"));
        assert!(served.headers.contains(&("content-range".into(), "bytes */10".into())));
    }

    #[test]
    fn a_fresh_copy_is_a_304_with_no_body() {
        let request = Request { method: "GET", range: None, if_modified_since: Some("Thu, 01 Jan 1970 00:00:00 GMT") };
        let served = serve_file(&request, &STAT, None, None);
        assert_eq!(served.status, 304);
        assert!(served.body.is_empty());
    }
}
