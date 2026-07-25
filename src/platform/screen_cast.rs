//! GNOME/Wayland screen capture through the XDG ScreenCast portal.
//!
//! The old implementation invoked `gdbus` and ImageMagick to take a desktop
//! screenshot.  That bypassed the compositor permission model and could
//! capture the wrong display.  This module owns one portal session and keeps
//! a PipeWire stream alive for the duration of a screen-share request.

use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SourceType},
    PersistMode,
};
use futures::executor::block_on;
use pipewire as pw;
use pw::{properties::properties, spa};
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Once,
};
use std::thread;
use std::time::Duration;

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
struct RawFrame {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    stride: usize,
    format: spa::param::video::VideoFormat,
}

/// A portal-authorized PipeWire capture session.
pub struct ScreenCastCapture {
    receiver: Receiver<Result<RawFrame, String>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl ScreenCastCapture {
    /// Ask the compositor for monitor capture permission and start a bounded
    /// PipeWire frame queue.  The permission dialog is shown at most once for
    /// this session, rather than once per frame.
    pub fn new() -> Result<Self, String> {
        let (node_id, fd) = block_on(open_portal())?;
        let (sender, receiver) = mpsc::sync_channel(2);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("desklink-screen-capture".to_string())
            .spawn(move || {
                if let Err(error) = run_pipewire(node_id, fd, sender.clone(), worker_stop) {
                    let _ = sender.send(Err(error));
                }
            })
            .map_err(|error| format!("Could not start screen capture worker: {error}"))?;

        Ok(Self {
            receiver,
            stop,
            worker: Some(worker),
        })
    }

    /// Return the next frame as a PNG.  A timeout is intentional: a wedged
    /// compositor or disconnected PipeWire graph must become a visible error,
    /// not hang the device session forever.
    pub fn next_png(&self, timeout: Duration) -> Result<(Vec<u8>, u32, u32), String> {
        let frame = self
            .receiver
            .recv_timeout(timeout)
            .map_err(|error| format!("Screen capture did not produce a frame: {error}"))??;
        encode_png(frame)
    }

    /// Return one compositor-authorized frame as tightly packed RGBA pixels.
    /// WebRTC media senders use this path directly; it avoids encoding a PNG
    /// and then wrapping that image in a second transport payload.
    pub fn next_rgba(&self, timeout: Duration) -> Result<(Vec<u8>, u32, u32), String> {
        let frame = self
            .receiver
            .recv_timeout(timeout)
            .map_err(|error| format!("Screen capture did not produce a frame: {error}"))??;
        let width = frame.width;
        let height = frame.height;
        Ok((to_rgba(&frame)?, width, height))
    }
}

impl Drop for ScreenCastCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn open_portal() -> Result<(u32, OwnedFd), String> {
    let proxy = Screencast::new()
        .await
        .map_err(|error| format!("ScreenCast portal unavailable: {error}"))?;
    let session = proxy
        .create_session()
        .await
        .map_err(|error| format!("Could not create ScreenCast session: {error}"))?;
    proxy
        .select_sources(
            &session,
            CursorMode::Hidden,
            SourceType::Monitor.into(),
            false,
            None,
            // Keep the compositor grant until the user revokes it. This
            // prevents a new permission request for every screen session.
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .map_err(|error| format!("Screen capture source selection was denied: {error}"))?;
    let response = proxy
        .start(&session, None)
        .await
        .map_err(|error| format!("Could not start ScreenCast session: {error}"))?
        .response()
        .map_err(|error| format!("ScreenCast session did not start: {error}"))?;
    let stream = response
        .streams()
        .first()
        .ok_or_else(|| "ScreenCast portal returned no monitor stream".to_string())?;
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session)
        .await
        .map_err(|error| format!("Could not open the PipeWire screen stream: {error}"))?;
    Ok((node_id, fd))
}

fn run_pipewire(
    node_id: u32,
    fd: OwnedFd,
    sender: SyncSender<Result<RawFrame, String>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    static PIPEWIRE_INIT: Once = Once::new();
    PIPEWIRE_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| format!("Could not create PipeWire main loop: {error}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|error| format!("Could not create PipeWire context: {error}"))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|error| format!("Could not connect to the portal PipeWire remote: {error}"))?;

    let loop_weak = mainloop.downgrade();
    let timer_loop = mainloop.downgrade();
    let timer_stop = Arc::clone(&stop);
    let timer = mainloop.loop_().add_timer(move |_| {
        if timer_stop.load(Ordering::Relaxed) {
            if let Some(loop_) = timer_loop.upgrade() {
                loop_.quit();
            }
        }
    });
    timer
        .update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        )
        .into_result()
        .map_err(|error| format!("Could not arm PipeWire shutdown timer: {error}"))?;

    let stream = pw::stream::StreamRc::new(
        core,
        "desklink-screen-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|error| format!("Could not create PipeWire screen stream: {error}"))?;

    let data = CaptureData {
        format: Default::default(),
        sender,
        stop,
        loop_weak,
    };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| {
            if matches!(new, pw::stream::StreamState::Error(_)) {
                eprintln!("[DeskLink] PipeWire screen stream changed from {old:?} to {new:?}");
            }
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id == pw::spa::param::ParamType::Format.as_raw() {
                let _ = user_data.format.parse(param);
            }
        })
        .process(|stream, user_data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let offset = chunk.offset() as usize;
            let size = chunk.size() as usize;
            let stride = chunk.stride();
            if stride <= 0 {
                return;
            }
            let width = user_data.format.size().width;
            let height = user_data.format.size().height;
            if width == 0 || height == 0 || size > MAX_FRAME_BYTES {
                return;
            }
            let Some(bytes) = data.data() else {
                return;
            };
            let end = offset.saturating_add(size);
            if end > bytes.len() {
                return;
            }
            let raw = RawFrame {
                bytes: bytes[offset..end].to_vec(),
                width,
                height,
                stride: stride as usize,
                format: user_data.format.format(),
            };
            match user_data.sender.try_send(Ok(raw)) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => {
                    user_data.stop.store(true, Ordering::Relaxed);
                    if let Some(loop_) = user_data.loop_weak.upgrade() {
                        loop_.quit();
                    }
                }
            }
        })
        .register()
        .map_err(|error| format!("Could not register PipeWire screen stream: {error}"))?;

    let format = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::RGB,
            pw::spa::param::video::VideoFormat::BGR
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 320,
                height: 240
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 12, denom: 1 },
            pw::spa::utils::Fraction { num: 1, denom: 1 },
            pw::spa::utils::Fraction { num: 30, denom: 1 }
        )
    );
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(format),
    )
    .map_err(|error| format!("Could not serialize PipeWire format: {error}"))?
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| "Could not create PipeWire format pod from serialized bytes".to_string())?];
    stream
        .connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|error| format!("Could not connect PipeWire screen stream: {error}"))?;
    mainloop.run();
    Ok(())
}

struct CaptureData {
    format: spa::param::video::VideoInfoRaw,
    sender: SyncSender<Result<RawFrame, String>>,
    stop: Arc<AtomicBool>,
    loop_weak: pw::main_loop::MainLoopWeak,
}

fn encode_png(frame: RawFrame) -> Result<(Vec<u8>, u32, u32), String> {
    let rgba = to_rgba(&frame)?;
    let mut output = Vec::new();
    let mut encoder = png::Encoder::new(&mut output, frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("Could not encode screen frame: {error}"))?;
    writer
        .write_image_data(&rgba)
        .map_err(|error| format!("Could not encode screen frame: {error}"))?;
    drop(writer);
    Ok((output, frame.width, frame.height))
}

fn to_rgba(frame: &RawFrame) -> Result<Vec<u8>, String> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let channels = if frame.format == spa::param::video::VideoFormat::RGB
        || frame.format == spa::param::video::VideoFormat::BGR
    {
        3
    } else {
        4
    };
    let row_bytes = width
        .checked_mul(channels)
        .ok_or_else(|| "Screen frame dimensions overflowed".to_string())?;
    if frame.stride < row_bytes || frame.bytes.len() < frame.stride.saturating_mul(height) {
        return Err("PipeWire screen frame has an invalid stride".to_string());
    }
    let output_len = width
        .checked_mul(height)
        .and_then(|size| size.checked_mul(4))
        .ok_or_else(|| "Screen frame dimensions overflowed".to_string())?;
    if output_len > MAX_FRAME_BYTES {
        return Err("Screen frame is too large".to_string());
    }
    let mut output = vec![0; output_len];
    for y in 0..height {
        for x in 0..width {
            let source = y * frame.stride + x * channels;
            let destination = (y * width + x) * 4;
            match frame.format {
                f if f == spa::param::video::VideoFormat::RGBA => {
                    output[destination..destination + 4]
                        .copy_from_slice(&frame.bytes[source..source + 4]);
                }
                f if f == spa::param::video::VideoFormat::RGBx => {
                    output[destination..destination + 4].copy_from_slice(&[
                        frame.bytes[source],
                        frame.bytes[source + 1],
                        frame.bytes[source + 2],
                        255,
                    ]);
                }
                f if f == spa::param::video::VideoFormat::BGRx => {
                    output[destination..destination + 4].copy_from_slice(&[
                        frame.bytes[source + 2],
                        frame.bytes[source + 1],
                        frame.bytes[source],
                        255,
                    ]);
                }
                f if f == spa::param::video::VideoFormat::RGB => {
                    output[destination..destination + 4].copy_from_slice(&[
                        frame.bytes[source],
                        frame.bytes[source + 1],
                        frame.bytes[source + 2],
                        255,
                    ]);
                }
                f if f == spa::param::video::VideoFormat::BGR => {
                    output[destination..destination + 4].copy_from_slice(&[
                        frame.bytes[source + 2],
                        frame.bytes[source + 1],
                        frame.bytes[source],
                        255,
                    ]);
                }
                _ => return Err("PipeWire supplied an unsupported screen pixel format".to_string()),
            }
        }
    }
    Ok(output)
}
