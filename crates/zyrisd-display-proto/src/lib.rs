//! Framing between the zyrisd parent and the desktop child.
//!
//! One frame = `[u32 BE json_len][json][u32 BE blob_len][blob]`.
//! Screenshot bytes ride the blob frame, so there is no base64 bloat.
//!
//! **The child's stdout is frames only, logs go to stderr.** Mix them and the framing breaks.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// A u32 reaches 4 GiB, so without a cap one corrupt header makes us try to allocate that much.
pub const JSON_MAX: usize = 1 << 20;
pub const BLOB_MAX: usize = 8 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Request {
    /// Actively checks whether a display really exists and whether input works.
    Probe,
    ListDisplays,
    Screenshot {
        display: Option<u32>,
        region: Option<Region>,
        format: Option<String>,
        max_width: Option<u32>,
    },
    MoveTo {
        x: i32,
        y: i32,
    },
    Click {
        button: String,
    },
    Key {
        key: String,
    },
    Type {
        text: String,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// Why the screenshot response carries meta: the capability's return is not a raw blob but
/// `Datum::Image{name, description, media_type, blob}`, and `description` is something only
/// the child can compute — once it downscales to fit the budget, image and display coordinates
/// diverge, and `description` is where that scale is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMeta {
    pub resolved_display_id: u32,
    pub sent_width: u32,
    pub sent_height: u32,
    pub media_type: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Response {
    Probe { displays: Vec<DisplayInfo>, input_ok: bool },
    Displays { displays: Vec<DisplayInfo> },
    Image { meta: ImageMeta },
    Ok,
    Error { message: String },
}

#[derive(Debug)]
pub struct Frame {
    pub id: u64,
    pub body: serde_json::Value,
    pub blob: Vec<u8>,
}

pub fn write_frame<W: Write>(
    w: &mut W,
    id: u64,
    body: &impl Serialize,
    blob: &[u8],
) -> io::Result<()> {
    let envelope = serde_json::json!({ "id": id, "body": serde_json::to_value(body)? });
    let json = serde_json::to_vec(&envelope)?;
    if json.len() > JSON_MAX || blob.len() > BLOB_MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds the limit"));
    }
    w.write_all(&(json.len() as u32).to_be_bytes())?;
    w.write_all(&json)?;
    w.write_all(&(blob.len() as u32).to_be_bytes())?;
    w.write_all(blob)?;
    w.flush()
}

fn read_len<R: Read>(r: &mut R, max: usize) -> io::Result<usize> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    let len = u32::from_be_bytes(buf) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length exceeds the limit"));
    }
    Ok(len)
}

/// A truncated frame ends in `UnexpectedEof`. The parent has to see that and fail the in-flight
/// request at once, or the caller hangs until it times out.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Frame> {
    let json_len = read_len(r, JSON_MAX)?;
    let mut json = vec![0u8; json_len];
    r.read_exact(&mut json)?;
    let envelope: serde_json::Value = serde_json::from_slice(&json)?;
    let id = envelope.get("id").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let body = envelope.get("body").cloned().unwrap_or(serde_json::Value::Null);

    let blob_len = read_len(r, BLOB_MAX)?;
    let mut blob = vec![0u8; blob_len];
    r.read_exact(&mut blob)?;
    Ok(Frame { id, body, blob })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_with_a_blob() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 7, &Request::Probe, b"\x01\x02\x03").unwrap();
        let got = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(got.id, 7);
        assert_eq!(got.blob, b"\x01\x02\x03");
        assert!(matches!(serde_json::from_value(got.body).unwrap(), Request::Probe));
    }

    #[test]
    fn an_empty_blob_is_fine() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 1, &Response::Ok, &[]).unwrap();
        assert!(read_frame(&mut buf.as_slice()).unwrap().blob.is_empty());
    }

    /// A u32 length reaches 4 GiB, so with no cap one corrupt header has the parent trying to
    /// allocate that much.
    #[test]
    fn an_oversized_length_is_refused_before_allocating() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(JSON_MAX as u32 + 1).to_be_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        let err = read_frame(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// A child that dies mid-response leaves a partial frame. Ending in UnexpectedEof is what lets
    /// the parent fail the in-flight request at once — otherwise the caller hangs until timeout.
    #[test]
    fn a_truncated_frame_reports_eof() {
        let mut whole = Vec::new();
        write_frame(&mut whole, 1, &Response::Ok, b"abcdefgh").unwrap();
        whole.truncate(whole.len() - 3);
        let err = read_frame(&mut whole.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// The image meta has to round-trip for the parent to assemble a Datum::Image.
    #[test]
    fn an_image_response_carries_the_metadata_only_the_child_can_compute() {
        let meta = ImageMeta {
            resolved_display_id: 1,
            sent_width: 1920,
            sent_height: 1080,
            media_type: "image/jpeg".into(),
            description: "Display 1 (3840x2160 scaled down 0.5x)".into(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, 3, &Response::Image { meta: meta.clone() }, b"jpegbytes").unwrap();
        let got = read_frame(&mut buf.as_slice()).unwrap();
        match serde_json::from_value(got.body).unwrap() {
            Response::Image { meta: back } => assert_eq!(back, meta),
            other => panic!("{other:?}"),
        }
        assert_eq!(got.blob, b"jpegbytes");
    }
}
