//! Desktop helper for zyrisd. The only binary that links graphics libraries.
//!
//! Reads frames from stdin, answers on stdout. **stdout is framing only; logs go to stderr.**
//! It exits when stdin closes, so a dead parent never leaves an orphan.
//!
//! Two reasons this is its own process. (1) A binary built with `desktop` links X11/Wayland/
//! mesa/pipewire as `DT_NEEDED`, so on a headless machine without those `.so`s it dies
//! before `main` is even reached — the parent must not carry that risk. (2) Enumerating
//! displays panics on some compositors, and that panic must not take the daemon down.

// Trait methods need the trait in scope.
use zyris_caps::{Input, ScreenCapture};
use zyrisd_display_proto::{read_frame, write_frame, ImageMeta, Request, Response};

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zyrisd_display=info".into()),
        )
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Cannot build the tokio runtime: {e}");
            std::process::exit(1);
        }
    };

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    // One request at a time. Look-then-act is inherently serial for the agent, and
    // multiplexing buys nothing for the cost of making the child concurrent.
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(f) => f,
            // Parent is gone. Exit quietly.
            Err(_) => return,
        };
        let (response, blob) = match serde_json::from_value::<Request>(frame.body) {
            Ok(req) => runtime.block_on(handle(req)),
            Err(e) => (Response::Error { message: e.to_string() }, Vec::new()),
        };
        if write_frame(&mut writer, frame.id, &response, &blob).is_err() {
            return;
        }
    }
}

async fn handle(req: Request) -> (Response, Vec<u8>) {
    match req {
        Request::Probe => {
            let displays = probe_displays();
            // Announce only what actually works. Advertising what we cannot do and failing
            // every call is worse than not showing up — no way to tell "absent" from "broken".
            //
            // Capture and input are checked separately: GNOME reports `wl_output` fine but
            // has no `zwlr_screencopy`, so enumeration succeeds and every capture fails.
            // That exact combination was observed on this machine.
            let screen_ok = !displays.is_empty() && capture_works().await;
            let input_ok = !displays.is_empty() && input_works();
            (Response::Probe { displays, screen_ok, input_ok }, Vec::new())
        }
        Request::ListDisplays => (Response::Displays { displays: probe_displays() }, Vec::new()),
        Request::Screenshot { display, region, format, max_width } => {
            match screen().screenshot(display, region, format, max_width).await {
                Ok(datum) => split_image(datum),
                Err(e) => (err(e), Vec::new()),
            }
        }
        // Build the input backend fresh on every call. `enigo`'s connection dies when the
        // display server restarts, and a long-held one makes all later input fail silently.
        // In a daemon that runs for weeks, reopening each time is the right call.
        Request::TypeText { text } => {
            unit(async move { make_input()?.type_text(text).await }.await)
        }
        Request::Key { chord } => unit(async move { make_input()?.key(chord).await }.await),
        Request::MoveTo { display, x, y } => {
            unit(async move { make_input()?.move_to(display, x, y).await }.await)
        }
        Request::Click { button } => unit(async move { make_input()?.click(button).await }.await),
        Request::Scroll { dx, dy } => unit(async move { make_input()?.scroll(dx, dy).await }.await),
    }
}

/// The backend proven to actually capture. Once found it sticks for the life of this process.
static BACKEND: std::sync::OnceLock<zyris_capkit::ScreenBackend> = std::sync::OnceLock::new();

/// Backends to try, starting with whatever `detect()` picked.
///
/// **Do not trust `detect()`.** It only checks that `WayshotConnection::new()` opens and
/// that `wl_output` is non-empty. GNOME satisfies both while implementing none of the
/// `zwlr_screencopy` capture needs. So the Wayland backend gets picked and every capture
/// fails with "Cannot find required wayland protocol" — observed on this machine.
/// xcap reaches the GNOME/KDE screenshot portal, so that is the answer.
#[cfg(target_os = "linux")]
fn backend_candidates() -> Vec<zyris_capkit::ScreenBackend> {
    use zyris_capkit::ScreenBackend;
    let first = ScreenBackend::detect();
    let mut all = vec![first];
    all.extend([ScreenBackend::Wayland, ScreenBackend::Xcap].into_iter().filter(|b| *b != first));
    all
}

#[cfg(not(target_os = "linux"))]
fn backend_candidates() -> Vec<zyris_capkit::ScreenBackend> {
    vec![zyris_capkit::ScreenBackend::detect()]
}

fn screen_with(backend: zyris_capkit::ScreenBackend) -> zyris_capkit::HostScreenCapture {
    // Keep the upstream downscale budget. Without it a 4K capture will not fit the response.
    zyris_capkit::HostScreenCapture::default().with_backend(backend)
}

/// Capturer built on the proven backend. Before one is proven, start with `detect()`'s pick.
fn screen() -> zyris_capkit::HostScreenCapture {
    screen_with(*BACKEND.get().unwrap_or(&backend_candidates()[0]))
}

/// Splits a `Datum::Image` into metadata and bytes.
///
/// `Blob` serializes as base64 on the wire, so shipping a finished `Datum` as JSON would
/// defeat the point of having a blob frame.
fn split_image(datum: zyris::Datum) -> (Response, Vec<u8>) {
    match datum {
        zyris::Datum::Image { name, description, media_type, blob } => {
            let bytes = blob.as_inline().map(|b| b.to_vec()).unwrap_or_default();
            (Response::Image { meta: ImageMeta { name, description, media_type } }, bytes)
        }
        other => (
            Response::Error { message: format!("Got a non-image datum: {other:?}") },
            Vec::new(),
        ),
    }
}

fn err(e: zyris::WireError) -> Response {
    Response::Error { message: e.to_string() }
}

fn unit(r: zyris::Result<()>) -> (Response, Vec<u8>) {
    match r {
        Ok(()) => (Response::Ok, Vec::new()),
        Err(e) => (err(e), Vec::new()),
    }
}

/// **Actually enumerates** the displays. That is the only probe worth trusting.
///
/// `ScreenBackend::detect()` returns Xcap with no probing at all when `WAYLAND_DISPLAY` is
/// unset, and enumeration down that path panics on some compositors. Caught here and turned
/// into an empty list — that means no screens, so the parent announces nothing.
fn probe_displays() -> Vec<zyris_caps::Display> {
    use zyris_capkit::Displays;
    match std::panic::catch_unwind(|| zyris_capkit::HostDisplays::default().displays()) {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "Failed to enumerate displays");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!("Display enumeration panicked; treating it as no screens");
            Vec::new()
        }
    }
}

fn make_input() -> zyris::Result<zyris_capkit::EnigoInput> {
    zyris_capkit::EnigoInput::new(zyris_capkit::HostDisplays::default())
}

/// Checks that input actually opens. Announce what cannot open and the tool list keeps an
/// entry that fails every call, with no way to tell "absent" from "broken".
fn input_works() -> bool {
    std::panic::catch_unwind(|| make_input().is_ok()).unwrap_or(false)
}

/// **Takes a tiny real capture with each backend and remembers the one that works.**
///
/// Enumerating displays is not enough, and neither is `detect()` — see the `backend_candidates`
/// comment above. Minimum width keeps the probe cheap; only success counts, the image is dropped.
async fn capture_works() -> bool {
    if BACKEND.get().is_some() {
        return true;
    }
    for backend in backend_candidates() {
        match screen_with(backend)
            .screenshot(None, None, Some(zyris_caps::ImageFormat::Jpeg), Some(64))
            .await
        {
            Ok(_) => {
                tracing::info!(?backend, "Screen capture works with this backend");
                let _ = BACKEND.set(backend);
                return true;
            }
            Err(e) => tracing::info!(?backend, error = %e, "Capture fails with this backend"),
        }
    }
    tracing::info!("No backend can capture, so screen_capture is not announced");
    false
}
