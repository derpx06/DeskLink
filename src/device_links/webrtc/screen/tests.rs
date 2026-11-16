//! Tests of the screen module: caps and TWCC, decoder order, parking, inbound
//! statistics, the bitrate and fps controller, the encoder chain and the
//! screen request's limits. Media over a real peer is tested in
//! `peer_connection::tests`.

use super::*;

/// The phone's H.264 never goes to a decoder that holds each frame until the next one
/// (`vah264dec`, measured 197 ms a frame at 5 fps) while another decoder can take it.
#[test]
fn a_decoder_that_holds_a_frame_is_tried_last() {
    let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    assert_eq!(
        low_latency_order(&names(&["vah264dec", "avdec_h264", "openh264dec"])),
        Some(vec![1, 2, 0])
    );
    // Nothing else to try, or nothing held: the order stays.
    assert_eq!(low_latency_order(&names(&["vah264dec"])), None);
    assert_eq!(low_latency_order(&names(&["avdec_h264", "vavp8dec"])), None);
    assert_eq!(low_latency_order(&names(&["vavp8dec", "vp8dec"])), None);
}

/// A phone's H.264 frame reaches the display when it arrives, not when the
/// next one does. libwebrtc's encoders (x264 here) send `pic_order_cnt_type`
/// 2, and `vah264dec` held each such frame until the next: one frame
/// interval of lag at every rate, seconds on a still screen.
#[test]
fn a_phone_h264_frame_is_shown_without_waiting_for_the_next() {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    if gst::init().is_err()
        || ["videotestsrc", "x264enc", "rtph264pay", "decodebin"]
            .iter()
            .any(|name| gst::ElementFactory::find(name).is_none())
        || !h264_decoder_available()
    {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let make = |name: &str| gst::ElementFactory::make(name).build().unwrap();
    let source = make("videotestsrc");
    source.set_property("is-live", true);
    source.set_property_from_str("pattern", "ball");
    source.set_property("num-buffers", 16i32);
    let raw = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", 864i32)
                .field("height", 1920i32)
                .field("framerate", gst::Fraction::new(5, 1))
                .build(),
        )
        .build()
        .unwrap();
    let encoder = make("x264enc");
    encoder.set_property_from_str("tune", "zerolatency");
    encoder.set_property_from_str("speed-preset", "ultrafast");
    let payloader = make("rtph264pay");
    payloader.set_property("config-interval", -1i32);
    let rtp = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("application/x-rtp")
                .field("media", "video")
                .field("encoding-name", "H264")
                .field("clock-rate", 90_000i32)
                .field("payload", H264_PAYLOAD)
                .build(),
        )
        .build()
        .unwrap();
    let chain = [&source, &raw, &encoder, &payloader, &rtp];
    pipeline.add_many(chain).unwrap();
    gst::Element::link_many(chain).unwrap();
    let pad = rtp.static_pad("src").unwrap();
    let arrived: Arc<Mutex<HashMap<u64, Instant>>> = Arc::default();
    {
        let arrived = Arc::clone(&arrived);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(pts) = info.buffer().and_then(|buffer| buffer.pts()) {
                arrived
                    .lock()
                    .unwrap()
                    .insert(pts.nseconds(), Instant::now());
            }
            gst::PadProbeReturn::Ok
        });
    }
    let stream = attach_incoming_video(&pipeline, &pad).unwrap();
    let waits: Arc<Mutex<Vec<f64>>> = Arc::default();
    {
        let (arrived, waits) = (Arc::clone(&arrived), Arc::clone(&waits));
        stream
            .sink_pad()
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let at = info
                    .buffer()
                    .and_then(|buffer| buffer.pts())
                    .and_then(|pts| arrived.lock().unwrap().get(&pts.nseconds()).copied());
                if let Some(at) = at {
                    waits.lock().unwrap().push(at.elapsed().as_secs_f64() * 1e3);
                }
                gst::PadProbeReturn::Ok
            });
    }
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let _ = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(20),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    std::thread::sleep(Duration::from_millis(300));
    let caps = stream.sink_pad().unwrap().current_caps().unwrap();
    pipeline.set_state(gst::State::Null).unwrap();
    // A phone screen is sRGB whatever the H.264 stream's colour description says.
    assert_eq!(
        sink_colorimetry(&caps).transfer(),
        gst_video::VideoTransferFunction::Srgb
    );
    let mut waits = waits.lock().unwrap().split_off(0);
    assert!(waits.len() >= 8, "only {} frames decoded", waits.len());
    // The first frames are the decoder starting up.
    let mut steady = waits.split_off(3);
    steady.sort_by(f64::total_cmp);
    let median = steady[steady.len() / 2];
    // A held frame waits a whole frame interval: 200 ms at 5 fps.
    assert!(
        median < 60.0,
        "median {median:.1} ms from arrival to display"
    );
}

/// The stats line's `udp drops` reads the kernel's counter on Linux.
#[test]
fn the_kernels_udp_receive_buffer_drops_are_read() {
    assert!(udp_receive_buffer_drops().is_some());
}

/// The stats line reads the phone's stream from rtpbin's sources: the remote
/// sender with the most bytes, never this desk's own (internal) source.
#[test]
fn inbound_stats_come_from_the_phones_biggest_source() {
    if gst::init().is_err() {
        return;
    }
    let source = |internal: bool, sender: bool, octets: u64| {
        gst::Structure::builder("application/x-rtp-source-stats")
            .field("internal", internal)
            .field("is-sender", sender)
            .field("octets-received", octets)
            .field("packets-lost", 3i32)
            .field("sent-nack-count", 4u32)
            .field("sent-pli-count", 1u32)
            .field("jitter", 900u32)
            .field("clock-rate", 90_000i32)
            .build()
    };
    let summary = inbound_summary(
        [
            source(true, true, 9_000_000),
            source(false, true, 40_000),
            source(false, true, 5_000_000),
            source(false, false, 7_000_000),
        ]
        .into_iter(),
    );
    assert_eq!(
        summary,
        Some(Inbound {
            bytes: 5_000_000,
            lost: 3,
            nacks: 4,
            plis: 1,
            jitter_ms: Some(10.0),
        })
    );
    assert_eq!(inbound_summary([source(true, true, 1)].into_iter()), None);
}

/// F28: the RTP session outlives a share, so a second share's stats line
/// counts loss, NACKs and PLIs from its own start, not the first share's.
#[test]
fn a_shares_stats_count_from_its_own_start() {
    let at = |bytes, lost, nacks, plis| Inbound {
        bytes,
        lost,
        nacks,
        plis,
        jitter_ms: Some(2.0),
    };
    assert_eq!(
        at(9_000, 90, 710, 175).since(&at(8_000, 85, 706, 172)),
        at(9_000, 5, 4, 3)
    );
}

/// A capture that tags its picture as rotated or flipped (PipeWire's video
/// transform) comes out turned the right way; an untagged one is untouched.
#[test]
fn the_capture_orientation_tag_turns_the_picture_the_right_way() {
    if gst::init().is_err() || gst::ElementFactory::find("taginject").is_none() {
        return;
    }
    let Some(_) = orientation_filter() else {
        return;
    };
    const W: usize = 64;
    const H: usize = 32;
    fn run(tag: Option<&str>) -> (usize, usize, Vec<u8>) {
        let description = format!(
            "videotestsrc num-buffers=1 pattern=smpte ! video/x-raw,format=GRAY8,width={W},height={H} ! {} \
                 videoflip video-direction=auto ! fakesink name=out sync=false",
            tag.map_or(String::new(), |tag| format!(
                "taginject tags=image-orientation={tag} !"
            ))
        );
        let pipeline = gst::parse::launch(&description)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let taken = Arc::new(Mutex::new(None));
        let probe = Arc::clone(&taken);
        pipeline
            .by_name("out")
            .unwrap()
            .static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                    let caps = pad.current_caps().unwrap();
                    let shape = caps.structure(0).unwrap();
                    let (w, h) = (
                        shape.get::<i32>("width").unwrap() as usize,
                        shape.get::<i32>("height").unwrap() as usize,
                    );
                    let pixels = buffer.map_readable().unwrap().to_vec();
                    *probe.lock().unwrap() = Some((w, h, pixels));
                }
                gst::PadProbeReturn::Ok
            });
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        pipeline.set_state(gst::State::Null).unwrap();
        let frame = taken.lock().unwrap().take();
        frame.expect("a frame")
    }
    let (_, _, reference) = run(None);
    let at = |pixels: &[u8], w: usize, x: usize, y: usize| pixels[y * w + x];
    // Not all one grey, or the checks below prove nothing.
    assert!(reference.windows(2).any(|pair| pair[0] != pair[1]));

    let (w, h, upside_down) = run(Some("rotate-180"));
    assert_eq!((w, h), (W, H));
    for (x, y) in [(0, 0), (W - 1, 0), (5, H - 1), (40, 7), (W - 1, H - 1)] {
        assert_eq!(
            at(&upside_down, w, x, y),
            at(&reference, W, W - 1 - x, H - 1 - y)
        );
    }
    let (_, _, mirrored) = run(Some("flip-rotate-0"));
    let (_, _, vertical) = run(Some("flip-rotate-180"));
    for (x, y) in [(0, 0), (W - 1, 0), (5, H - 1), (40, 7)] {
        assert_eq!(
            at(&mirrored, W, x, y),
            at(&reference, W, W - 1 - x, y),
            "horizontal"
        );
        assert_eq!(
            at(&vertical, W, x, y),
            at(&reference, W, x, H - 1 - y),
            "vertical"
        );
    }
    // Quarter turns swap the sides, either way round.
    for tag in ["rotate-90", "rotate-270"] {
        let (w, h, _) = run(Some(tag));
        assert_eq!((w, h), (H, W), "{tag}");
    }
    // Untouched when untagged, and an explicit "upright" tag changes nothing.
    let (_, _, upright) = run(Some("rotate-0"));
    assert_eq!(upright, reference);
}

#[test]
fn a_reshare_reads_the_phones_report_from_the_rtp_session() {
    gst::init().unwrap();
    let source = |ssrc: u32, internal: bool, have_rb: bool| {
        gst::Structure::builder("application/x-rtp-source-stats")
            .field("ssrc", ssrc)
            .field("internal", internal)
            .field("clock-rate", 90_000i32)
            .field("octets-sent", 4_000u64)
            .field("have-rb", have_rb)
            .field("rb-fractionlost", 64u32)
            .field("rb-jitter", 900u32)
            .field("rb-round-trip", 3_277u32)
            .build()
    };
    let (report, bytes) = report_for_source(
        [source(7, false, true), source(42, true, true)].into_iter(),
        42,
    )
    .expect("our own source, reported on");
    assert_eq!(bytes, 4_000);
    assert!((report.loss - 0.25).abs() < 1e-9);
    assert!((report.jitter_ms.unwrap() - 10.0).abs() < 1e-9);
    assert!((report.rtt_ms.unwrap() - 50.0).abs() < 0.1);
    assert!(report_for_source([source(42, true, false)].into_iter(), 42).is_none());
    assert!(report_for_source([source(42, false, true)].into_iter(), 42).is_none());
}

/// GStreamer 1.24 (Ubuntu 24.04) keeps the phone's report about our
/// stream on the phone's own source (`rb-ssrc` = ours), not on ours: a
/// reopened share there never raised its bitrate.
#[test]
fn a_reshare_reads_the_report_kept_on_the_phones_source_too() {
    gst::init().unwrap();
    let ours = gst::Structure::builder("application/x-rtp-source-stats")
        .field("ssrc", 42u32)
        .field("internal", true)
        .field("clock-rate", 90_000i32)
        .field("octets-sent", 4_000u64)
        .build();
    let phone = |rb_ssrc: u32| {
        gst::Structure::builder("application/x-rtp-source-stats")
            .field("ssrc", 7u32)
            .field("internal", false)
            .field("clock-rate", -1i32)
            .field("have-rb", true)
            .field("rb-ssrc", rb_ssrc)
            .field("rb-fractionlost", 64u32)
            .field("rb-jitter", 900u32)
            .field("rb-round-trip", 3_277u32)
            .build()
    };
    let (report, bytes) = report_for_source([phone(42), ours.clone()].into_iter(), 42)
        .expect("the phone's report about our stream");
    assert_eq!(bytes, 4_000);
    assert!((report.loss - 0.25).abs() < 1e-9);
    assert!((report.jitter_ms.unwrap() - 10.0).abs() < 1e-9);
    assert!((report.rtt_ms.unwrap() - 50.0).abs() < 0.1);
    // A report about another stream is not about ours.
    assert!(report_for_source([phone(9), ours].into_iter(), 42).is_none());
}

#[test]
fn the_rate_follows_loss_and_queueing_in_small_steps() {
    let clean = LinkReport {
        loss: 0.0,
        rtt_ms: Some(20.0),
        jitter_ms: Some(2.0),
    };
    let up = next_bitrate(START_BITRATE, &clean, 20.0);
    assert!(
        up > START_BITRATE && up <= START_BITRATE / 100 * 106,
        "a small probe up"
    );
    let lan = LinkReport {
        rtt_ms: Some(6.0),
        ..clean
    };
    let fast = next_bitrate(START_BITRATE, &lan, 6.0);
    assert!(
        fast > up && fast <= START_BITRATE / 100 * 116,
        "a LAN round trip climbs faster"
    );
    let queueing = LinkReport {
        rtt_ms: Some(120.0),
        ..clean
    };
    assert!(
        next_bitrate(START_BITRATE, &queueing, 20.0) < START_BITRATE,
        "delay backs off before loss"
    );
    let jittery = LinkReport {
        jitter_ms: Some(45.0),
        ..clean
    };
    assert!(next_bitrate(START_BITRATE, &jittery, 20.0) < START_BITRATE);
    let light_loss = LinkReport {
        loss: 0.05,
        ..clean
    };
    let heavy_loss = LinkReport {
        loss: 0.20,
        ..clean
    };
    let light = next_bitrate(START_BITRATE, &light_loss, 20.0);
    let heavy = next_bitrate(START_BITRATE, &heavy_loss, 20.0);
    assert!(
        heavy < light && light < START_BITRATE,
        "harder under real loss"
    );
    assert_eq!(next_bitrate(MIN_BITRATE, &heavy_loss, 20.0), MIN_BITRATE);
    assert_eq!(next_bitrate(MAX_BITRATE, &clean, 20.0), MAX_BITRATE);
    let floor = (0..20).fold(START_BITRATE, |rate, _| {
        next_bitrate(rate, &heavy_loss, 20.0)
    });
    assert_eq!(floor, MIN_BITRATE);
}

#[test]
fn frame_rate_drops_only_after_sustained_loss_at_the_floor_and_recovers() {
    let full = 60;
    let mut streak = 0;
    // Loss above the floor is the bitrate's job, not the frame rate's.
    for _ in 0..10 {
        assert_eq!(next_fps(full, full, START_BITRATE, 0.3, &mut streak), full);
    }
    let mut fps = full;
    for second in 1..=FPS_HOLD_SECONDS {
        fps = next_fps(fps, full, MIN_BITRATE, 0.3, &mut streak);
        assert_eq!(fps, if second < FPS_HOLD_SECONDS { full } else { 30 });
    }
    // A clean second in between resets the count: no flapping.
    assert_eq!(next_fps(fps, full, START_BITRATE, 0.0, &mut streak), 30);
    assert_eq!(next_fps(fps, full, START_BITRATE, 0.3, &mut streak), 30);
    for _ in 0..FPS_HOLD_SECONDS {
        fps = next_fps(fps, full, START_BITRATE, 0.0, &mut streak);
    }
    assert_eq!(fps, full);
    // Already slow: nothing lower than 10.
    assert_eq!(next_fps(10, 10, MIN_BITRATE, 0.5, &mut 99), 10);
}

/// Sharp and prompt: Lanczos (or VA) scaling to the phone's size, a frame
/// rate cap that only drops, caps that do not pin the rate, and VP8 tuned
/// for screens.
/// Once a share's VA scaler failed, every later chain scales in
/// software.
#[test]
fn the_encoder_chain_is_tuned_for_screens() {
    gst::init().unwrap();
    let request = ScreenRequest::from_parts(Some(2400), Some(60)).served(2560, 1600);
    assert_eq!(request.fps, 30, "software VP8 above 1920 px runs at 30");
    let (scale, convert, rate, filter, _queue, encoder, _payloader) =
        encoder_chain(2560, 1600, request).unwrap();
    assert_eq!(
        scale
            .factory()
            .map(|factory| factory.name().to_string())
            .as_deref(),
        Some("videoscale"),
        "never vapostproc: it capped the chain at 26-36 fps"
    );
    assert_eq!(scale.property::<u32>("n-threads"), 4);
    let method = scale.property_value("method");
    assert_eq!(
        gst::glib::EnumValue::from_value(&method).map(|(_, value)| value.nick()),
        Some("lanczos")
    );
    assert!(convert.has_property("n-threads", None));
    assert!(rate.property::<bool>("drop-only"));
    assert_eq!(rate.property::<i32>("max-rate"), 30);
    let caps = filter.property::<gst::Caps>("caps");
    let structure = caps.structure(0).unwrap();
    assert!(
        !structure.has_field("framerate"),
        "the rate is never pinned"
    );
    assert_eq!(structure.get::<i32>("width").unwrap(), 2400);
    assert_eq!(structure.get::<i32>("height").unwrap(), 1500);
    assert_eq!(encoder.property::<i32>("min-quantizer"), 10);
    assert_eq!(encoder.property::<i32>("max-quantizer"), 50);
    assert_eq!(encoder.property::<i32>("noise-sensitivity"), 0);
    assert_eq!(encoder.property::<i32>("static-threshold"), 0);
    assert_eq!(encoder.property::<i32>("cpu-used"), 5);
    // A phone that fits in 1920 gets its 60 fps.
    assert_eq!(
        ScreenRequest::from_parts(Some(1920), Some(60))
            .served(2560, 1600)
            .fps,
        60
    );
}

use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_SCREEN_REQUEST};

#[test]
fn screen_request_clamps_values_supplied_by_the_peer() {
    let mut packet = NetworkPacket::new(PACKET_TYPE_SCREEN_REQUEST);
    packet.set("maxDimension", 99_999);
    packet.set("fps", 240);

    let request = ScreenRequest::from_body(&packet);

    assert_eq!(request.max_dimension, 2560);
    assert_eq!(request.fps, 60);
    let small = ScreenRequest::from_parts(Some(100), Some(0));
    assert_eq!((small.max_dimension, small.fps), (640, 1));
}

#[test]
fn screen_request_falls_back_to_defaults() {
    let packet = NetworkPacket::new(PACKET_TYPE_SCREEN_REQUEST);
    assert_eq!(ScreenRequest::from_body(&packet), ScreenRequest::FALLBACK);
    assert_eq!(
        ScreenRequest::from_parts(None, None),
        ScreenRequest::FALLBACK
    );
}

#[test]
fn the_twcc_id_is_read_from_the_video_section_only() {
    let sdp = "v=0\r\nm=audio 9 X 111\r\na=extmap:9 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
                   m=video 9 X 96\r\na=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
                   a=extmap:4/sendrecv http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n";
    assert_eq!(twcc_extmap_id(sdp), Some(4));
    assert_eq!(twcc_extmap_id("v=0\r\nm=video 9 X 96\r\n"), None);
}

#[test]
fn the_answer_accepts_h264_then_vp8_with_the_offers_twcc_id() {
    gst::init().unwrap();
    let caps = answer_video_caps(Some(4), true);
    let names: Vec<_> = caps
        .iter()
        .map(|s| s.get::<&str>("encoding-name").unwrap().to_string())
        .collect();
    assert_eq!(names, ["H264", "VP8"]);
    assert!(caps
        .iter()
        .all(|s| s.get::<&str>("extmap-4").unwrap() == TWCC_URI));
    // No payload numbers: the offer's must win in the intersection.
    assert!(caps.iter().all(|s| !s.has_field("payload")));
    assert!(answer_video_caps(None, true)
        .iter()
        .all(|s| !s.has_field("extmap-4")));
}

#[test]
fn without_an_h264_decoder_only_vp8_is_offered_or_answered() {
    gst::init().unwrap();
    let names = |caps: gst::Caps| -> Vec<String> {
        caps.iter()
            .map(|s| s.get::<&str>("encoding-name").unwrap().to_string())
            .collect()
    };
    assert_eq!(names(video_caps(false)), ["VP8"]);
    assert_eq!(names(answer_video_caps(Some(4), false)), ["VP8"]);
    // The registry probe itself must not panic, whatever is installed.
    let _ = h264_decoder_available();
}

#[test]
fn video_caps_prefer_hardware_h264_and_keep_vp8_for_sending() {
    // Building caps touches the GStreamer type system, so this test needs
    // the library initialised even though it starts no pipeline.
    gst::init().unwrap();

    let caps = video_caps(true);
    let names: Vec<_> = caps
        .iter()
        .map(|s| s.get::<&str>("encoding-name").unwrap().to_string())
        .collect();
    assert_eq!(names, ["H264", "VP8"]);
    let h264 = caps.structure(0).unwrap();
    assert_eq!(h264.get::<&str>("packetization-mode").unwrap(), "1");
    assert!(caps
        .iter()
        .all(|s| s.get::<&str>("media").unwrap() == "video"));
}

#[test]
fn desktop_capture_dimensions_preserve_aspect_and_bound_encoding_cost() {
    assert_eq!(stream_dimensions(2560, 1600, 1280).unwrap(), (1280, 800));
    assert_eq!(stream_dimensions(1080, 2400, 1280).unwrap(), (576, 1280));
    assert_eq!(stream_dimensions(2560, 1600, 2400).unwrap(), (2400, 1500));
    assert_eq!(stream_dimensions(2560, 1600, 9999).unwrap(), (2560, 1600));
    assert!(stream_dimensions(0, 1080, 1280).is_err());
}

/// The decode chain goes, and the pad is parked on a fakesink rather than
/// left unlinked (RTP into an unlinked pad kills webrtcbin's receive path).
fn is_parked(pad: &gst::Pad) -> bool {
    pad.peer()
        .and_then(|peer| peer.parent_element())
        .and_then(|element| element.factory())
        .is_some_and(|factory| factory.name() == "fakesink")
}

#[test]
fn stopping_an_inbound_stream_removes_its_decode_chain_and_parks_the_pad() {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("identity").build().unwrap();
    pipeline.add(&source).unwrap();
    let source_pad = source.static_pad("src").unwrap();
    let stream = attach_incoming_video(&pipeline, &source_pad).unwrap();
    publish_stream("phone-stop", stream);
    assert_eq!(pipeline.children().len(), 4);

    clear_stream("phone-stop");

    assert_eq!(pipeline.children().len(), 2, "identity + parking fakesink");
    assert!(is_parked(&source_pad));
    forget_parked("phone-stop");
}

#[test]
fn a_stopped_view_is_shown_again_on_the_same_pad() {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("identity").build().unwrap();
    pipeline.add(&source).unwrap();
    let source_pad = source.static_pad("src").unwrap();
    publish_stream(
        "phone-again",
        attach_incoming_video(&pipeline, &source_pad).unwrap(),
    );
    clear_stream("phone-again");
    let asked = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&asked);
    // Seen where the request leaves the source upstream (towards the sender).
    source.static_pad("sink").unwrap().add_probe(
        gst::PadProbeType::EVENT_UPSTREAM,
        move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                if event
                    .structure()
                    .is_some_and(|s| s.name() == "GstForceKeyUnit")
                {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        },
    );

    // Live, as webrtcbin's pad is (a pad of a stopped element refuses events).
    pipeline.set_state(gst::State::Playing).unwrap();
    assert!(resume_parked("phone-again"));
    // The phone's encoder carries on from the first view: without a
    // keyframe the new decoder showed nothing for ~0.5-0.7 s (emulator).
    assert_eq!(
        asked.load(Ordering::Relaxed),
        1,
        "a keyframe is asked for at once"
    );
    assert!(stream_for("phone-again").is_some());
    assert_eq!(
        pipeline.children().len(),
        4,
        "decode chain back, fakesink gone"
    );
    assert!(!is_parked(&source_pad));
    assert!(!resume_parked("phone-again"), "nothing parked any more");
    clear_stream("phone-again");
    forget_parked("phone-again");
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn replacing_an_inbound_stream_stops_the_previous_decode_chain() {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let first_source = gst::ElementFactory::make("identity").build().unwrap();
    let second_source = gst::ElementFactory::make("identity").build().unwrap();
    pipeline.add_many([&first_source, &second_source]).unwrap();
    let first_pad = first_source.static_pad("src").unwrap();
    let first = attach_incoming_video(&pipeline, &first_pad).unwrap();
    publish_stream("phone-replace", first);
    let second =
        attach_incoming_video(&pipeline, &second_source.static_pad("src").unwrap()).unwrap();
    publish_stream("phone-replace", second);

    // first chain replaced by a parking fakesink; second chain live.
    assert_eq!(pipeline.children().len(), 6);
    assert!(is_parked(&first_pad));
    clear_stream("phone-replace");
    assert_eq!(pipeline.children().len(), 4);
    forget_parked("phone-replace");
}

#[test]
fn unlinked_inbound_track_stops_its_decode_chain() {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let source = gst::ElementFactory::make("identity").build().unwrap();
    pipeline.add(&source).unwrap();
    let source_pad = source.static_pad("src").unwrap();
    let stream = attach_incoming_video(&pipeline, &source_pad).unwrap();
    let decode_sink = stream.inner.elements[0].static_pad("sink").unwrap();

    source_pad.unlink(&decode_sink).unwrap();

    assert_eq!(pipeline.children().len(), 2, "identity + parking fakesink");
    assert!(is_parked(&source_pad));
}

/// The phone's sender report maps a frame's RTP timestamp back to its
/// capture time: 9000 ticks (100 ms) after the report, shown 130 ms after
/// the report, is 30 ms from capture to the sink.
#[test]
fn a_frame_is_timed_from_its_capture_by_the_sender_report() {
    // NTP 3_900_000_000.5 s = unix 1_691_011_200.5 s.
    let ntp = (3_900_000_000u64 << 32) | (1u64 << 31);
    let report_unix_ns = 1_691_011_200_500_000_000i128;
    let ms = capture_to_shown_ms(
        (ntp, 4_294_960_000),
        4_294_960_000u32.wrapping_add(9_000),
        report_unix_ns + 130_000_000,
    );
    assert!((ms - 30.0).abs() < 0.01, "{ms}");
    // A frame captured before the report (RTP timestamp behind it).
    let ms = capture_to_shown_ms((ntp, 100), 100u32.wrapping_sub(900), report_unix_ns);
    assert!((ms - 10.0).abs() < 0.01, "{ms}");
}

#[test]
fn the_sender_report_is_read_from_the_phones_video_source() {
    if gst::init().is_err() {
        return;
    }
    let source = |octets: u64, sr: bool| {
        gst::Structure::builder("application/x-rtp-source-stats")
            .field("internal", false)
            .field("is-sender", true)
            .field("octets-received", octets)
            .field("have-sr", sr)
            .field("sr-ntptime", octets)
            .field("sr-rtptime", 7u32)
            .build()
    };
    assert_eq!(
        sender_report([source(40, true), source(5_000, true)].into_iter()),
        Some((5_000, 7))
    );
    assert_eq!(sender_report([source(5_000, false)].into_iter()), None);
}

/// The harness folder gets frames of the phone's screen: only a real folder
/// of this user's that nobody else can write is taken, and files are made
/// 0600 and never written through a link.
#[test]
fn the_frame_folder_must_be_private_and_its_files_are() {
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("p34-frames-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir(&base).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(private_dir(&base));
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(!private_dir(&base), "world-writable");
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = base.with_extension("link");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&base, &link).unwrap();
    assert!(!private_dir(&link), "a link to a folder");

    let file = open_private(&base, "frames.csv", true).unwrap();
    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    std::os::unix::fs::symlink(base.join("frames.csv"), base.join("frame.png.tmp")).unwrap();
    assert!(
        open_private(&base, "frame.png.tmp", false).is_err(),
        "through a link"
    );
    let _ = std::fs::remove_file(&link);
    std::fs::remove_dir_all(&base).unwrap();
}

/// A phone's VP8 screen reaches the sink as what it is (sRGB, BT.601), not as
/// camera video: GTK converted the decoder's BT.709 transfer to sRGB and the
/// phone's grey 17 showed as 33.
#[test]
fn a_phone_vp8_screen_reaches_the_sink_as_srgb_bt601() {
    if gst::init().is_err()
        || ["videotestsrc", "vp8enc", "rtpvp8pay", "decodebin"]
            .iter()
            .any(|name| gst::ElementFactory::find(name).is_none())
    {
        return;
    }
    let pipeline = gst::Pipeline::new();
    let make = |name: &str| gst::ElementFactory::make(name).build().unwrap();
    let source = make("videotestsrc");
    source.set_property("num-buffers", 3i32);
    let raw = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .field("width", 864i32)
                .field("height", 1920i32)
                .build(),
        )
        .build()
        .unwrap();
    let encoder = make("vp8enc");
    let payloader = make("rtpvp8pay");
    let rtp = gst::ElementFactory::make("capsfilter")
        .property("caps", video_caps(false))
        .build()
        .unwrap();
    let chain = [&source, &raw, &encoder, &payloader, &rtp];
    pipeline.add_many(chain).unwrap();
    gst::Element::link_many(chain).unwrap();
    let stream = attach_incoming_video(&pipeline, &rtp.static_pad("src").unwrap()).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let _ = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(20),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let caps = stream
        .sink_pad()
        .unwrap()
        .current_caps()
        .expect("the sink got caps");
    pipeline.set_state(gst::State::Null).unwrap();
    let colorimetry = sink_colorimetry(&caps);
    assert_eq!(
        colorimetry.transfer(),
        gst_video::VideoTransferFunction::Srgb,
        "{caps}"
    );
    assert_eq!(
        colorimetry.matrix(),
        gst_video::VideoColorMatrix::Bt601,
        "{caps}"
    );
}

/// The colorimetry the sink was given, as the sink reads it (field, else
/// GStreamer's default for the size).
fn sink_colorimetry(caps: &gst::Caps) -> gst_video::VideoColorimetry {
    gst_video::VideoInfo::from_caps(caps).unwrap().colorimetry()
}

/// H.264 keeps the matrix and range its stream signals; only the transfer is
/// the screen's.
#[test]
fn a_phone_h264_screen_keeps_its_signalled_matrix_and_range() {
    use gst_video::{VideoColorMatrix, VideoColorRange, VideoColorimetry, VideoTransferFunction};
    gst::init().unwrap();
    let caps = |colorimetry: &str| {
        gst::Caps::builder("video/x-raw")
            .field("format", "I420")
            .field("colorimetry", colorimetry)
            .build()
    };
    let read = |caps: &gst::Caps| -> VideoColorimetry {
        caps.structure(0)
            .unwrap()
            .get::<&str>("colorimetry")
            .unwrap()
            .parse()
            .unwrap()
    };
    let fixed = screen_colorimetry(&caps("bt709"), false).unwrap();
    assert_eq!(read(&fixed).transfer(), VideoTransferFunction::Srgb);
    assert_eq!(read(&fixed).matrix(), VideoColorMatrix::Bt709);
    let full_601 = caps("1:4:5:4");
    let fixed = screen_colorimetry(&full_601, false).unwrap();
    assert_eq!(read(&fixed).range(), VideoColorRange::Range0_255);
    assert_eq!(read(&fixed).matrix(), VideoColorMatrix::Bt601);
    // Already right: left alone.
    assert!(screen_colorimetry(&fixed, false).is_none());
}
