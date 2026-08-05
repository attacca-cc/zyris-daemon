//! Framing between the zyrisd parent and the desktop child.
//!
//! One frame = `[u32 BE json_len][json][u32 BE blob_len][blob]`.
//! Screenshot bytes ride the blob frame, so there is no base64 bloat — `Blob` serializes to
//! base64 on the wire, so shipping a finished `Datum` as JSON would throw that saving away.
//!
//! **The child's stdout is frames only, logs go to stderr.** Mix them and the framing breaks.
//!
//! Messages carry the real `zyris-caps` types on purpose. Defining our own types and
//! translating between them would only create a place for fields to drift.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use zyris_caps::{Display, ImageFormat, MouseButton, Region};

/// A u32 reaches 4 GiB, so without a cap one corrupt header makes us try to allocate that much.
pub const JSON_MAX: usize = 1 << 20;
pub const BLOB_MAX: usize = 8 << 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Request {
    /// Checks whether capture and input **actually work**.
    ///
    /// Why we don't defer to capkit's backend selection: `ScreenBackend::detect()`
    /// returns Xcap with no probe at all when `WAYLAND_DISPLAY` is unset, and on some compositors
    /// it panics.
    ///
    /// Enumerating displays is not enough either — GNOME reports `wl_output` just fine while
    /// never implementing `zwlr_screencopy`, so enumeration succeeds and every capture fails.
    /// So the probe really does attempt one small capture.
    Probe,
    ListDisplays,
    Screenshot {
        display: Option<String>,
        region: Option<Region>,
        format: Option<ImageFormat>,
        max_width: Option<u32>,
    },
    TypeText {
        text: String,
    },
    Key {
        chord: String,
    },
    MoveTo {
        display: String,
        x: i32,
        y: i32,
    },
    Click {
        button: MouseButton,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
}

/// The child sends meta only instead of a whole `Datum::Image`, the bytes go out in the blob frame.
///
/// Only the child can compute that meta — once it downscales to fit the budget, image coordinates
/// no longer match display coordinates, and `description` is where that scale is recorded. Handed
/// nothing but a raw blob, the parent has no way to know what to write there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMeta {
    pub name: String,
    pub description: Option<String>,
    pub media_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Response {
    /// Keeping `screen_ok` and `input_ok` separate is the point. Displays that enumerate but
    /// refuse to capture are real (GNOME), so the parent reads both and picks what to announce.
    Probe { displays: Vec<Display>, screen_ok: bool, input_ok: bool },
    Displays { displays: Vec<Display> },
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

    fn a_display() -> Display {
        Display {
            id: "DP-1".into(),
            name: "Built-in display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            scale_factor: 1.0,
            primary: true,
        }
    }

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
            name: "DP-1.jpg".into(),
            description: Some("Display DP-1 (3840x2160 scaled down 0.5x)".into()),
            media_type: "image/jpeg".into(),
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

    /// The capability's real types cross unchanged — no translation layer, no place to drift.
    #[test]
    fn capability_types_cross_the_wire_unchanged() {
        let mut buf = Vec::new();
        let displays = vec![a_display()];
        let probe =
            Response::Probe { displays: displays.clone(), screen_ok: false, input_ok: true };
        write_frame(&mut buf, 1, &probe, &[]).unwrap();
        match serde_json::from_value(read_frame(&mut buf.as_slice()).unwrap().body).unwrap() {
            // Enumerates fine but captures not at all: that combination (GNOME) is real.
            Response::Probe { displays: back, screen_ok, input_ok } => {
                assert_eq!(back, displays);
                assert!(!screen_ok);
                assert!(input_ok);
            }
            other => panic!("{other:?}"),
        }

        let mut buf = Vec::new();
        let req = Request::Screenshot {
            display: Some("DP-1".into()),
            region: Some(Region { x: 1, y: 2, width: 3, height: 4 }),
            format: Some(ImageFormat::Jpeg),
            max_width: Some(1280),
        };
        write_frame(&mut buf, 2, &req, &[]).unwrap();
        let back: Request =
            serde_json::from_value(read_frame(&mut buf.as_slice()).unwrap().body).unwrap();
        assert_eq!(back, req);
    }
}
