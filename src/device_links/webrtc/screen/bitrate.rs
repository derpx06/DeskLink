//! The desktop share's bitrate and frame-rate controller: the link report from
//! the phone's receiver reports and the next bitrate and fps it gives.
//!
//! Sibling edges: `encoder` (start bitrate, setting the encoder's bitrate).

use super::*;

/// Encoder bitrate bounds for the desktop screen.
pub(super) const MIN_BITRATE: u32 = 400_000;
pub(super) const MAX_BITRATE: u32 = 12_000_000;

/// What the phone's receiver reports say about the link, once a second.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct LinkReport {
    /// 0..1.
    pub(crate) loss: f64,
    pub(crate) rtt_ms: Option<f64>,
    pub(crate) jitter_ms: Option<f64>,
}

/// Rate control on loss *and* delay: a queue building up (round trip above
/// the link's own baseline, or jitter) backs off before packets are lost.
/// Steps are small and smooth, except under real loss. `baseline_ms` is the
/// lowest round trip seen lately.
pub(super) fn next_bitrate(current: u32, report: &LinkReport, baseline_ms: f64) -> u32 {
    let queueing = report
        .rtt_ms
        .is_some_and(|rtt| rtt > baseline_ms * 1.5 + 20.0)
        || report.jitter_ms.is_some_and(|jitter| jitter > 30.0);
    let factor = if report.loss > 0.10 {
        0.75
    } else if report.loss > 0.02 {
        0.92
    } else if queueing {
        0.90
    } else if report.rtt_ms.is_some_and(|rtt| rtt < 20.0) {
        // A clean link a LAN away: up to 8 Mbps in ~8 s, not ~25 s. At 5% a
        // second the phone's view stayed soft for its first half minute
        // (measured: target 2.0 to 5.0 Mbps over 20 s at 5-15 ms rtt).
        1.15
    } else {
        1.05
    };
    ((f64::from(current) * factor) as u32).clamp(MIN_BITRATE, MAX_BITRATE)
}

/// Seconds of a condition before the frame rate changes, so it never flaps.
pub(super) const FPS_HOLD_SECONDS: u32 = 5;

/// The second rung after bitrate: when even the lowest bitrate still loses
/// packets, fewer frames give each one more bits (text stays sharp, motion
/// gets choppier); when the link is clean again with room to spare, the full
/// rate (`full`, what the phone asked for) returns. Counts consecutive
/// seconds of each condition in `streak`.
pub(super) fn next_fps(
    current: i32,
    full: i32,
    bitrate: u32,
    fraction_lost: f64,
    streak: &mut u32,
) -> i32 {
    let reduced = (full / 2).max(10).min(full);
    let wants_lower = current == full && bitrate == MIN_BITRATE && fraction_lost > 0.02;
    let wants_higher = current == reduced
        && current != full
        && bitrate >= START_BITRATE / 2
        && fraction_lost < 0.02;
    if wants_lower || wants_higher {
        *streak += 1;
    } else {
        *streak = 0;
    }
    if *streak < FPS_HOLD_SECONDS {
        return current;
    }
    *streak = 0;
    if wants_lower {
        reduced
    } else {
        full
    }
}

/// Once a second, reads the phone's receiver reports from `webrtcbin` stats
/// and steers the encoder's bitrate, then the frame rate through
/// `videorate`'s `max-rate` (no caps pin the rate, so nothing has to
/// renegotiate). Debug builds log a stats line every 2 s. Holds only weak
/// references, so it ends with the pipeline; the returned flag ends it with
/// the capture.
pub(super) fn spawn_bitrate_controller(
    webrtcbin: &gst::Element,
    encoder: &gst::Element,
    rate: &gst::Element,
    full_fps: i32,
    chain_stats: ChainStats,
    ssrc: Option<u32>,
) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let (webrtcbin, encoder, rate, flag) = (
        webrtcbin.downgrade(),
        encoder.downgrade(),
        rate.downgrade(),
        Arc::clone(&stop),
    );
    let spawned = std::thread::Builder::new()
        .name("desklink-screen-rate".to_string())
        .spawn(move || {
            let mut bitrate = START_BITRATE;
            let mut fps = full_fps;
            let mut streak = 0;
            let mut baseline_ms = f64::MAX;
            let mut tick = 0u64;
            let mut last_bytes = None;
            let mut silent_logged = false;
            while !flag.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                tick += 1;
                let (Some(webrtcbin), Some(encoder)) = (webrtcbin.upgrade(), encoder.upgrade())
                else {
                    break;
                };
                let (report, bytes_sent) = match link_report(&webrtcbin, ssrc) {
                    Ok(report) => report,
                    Err(why) => {
                        // Once per share: reopened shares on a phone logged
                        // no stats at all, and nothing said why (F8 follow-up).
                        if !silent_logged && tick >= 5 {
                            silent_logged = true;
                            debug_log!("[DeskLink] screen-stats: none yet, {why}");
                        }
                        continue;
                    }
                };
                if let Some(rtt) = report.rtt_ms {
                    // The baseline follows a link that got slower for good.
                    baseline_ms = (baseline_ms * 1.01).min(rtt);
                }
                let next = next_bitrate(bitrate, &report, baseline_ms);
                if next != bitrate {
                    bitrate = next;
                    set_encoder_bitrate(&encoder, bitrate);
                }
                let next = next_fps(fps, full_fps, bitrate, report.loss, &mut streak);
                if next != fps {
                    fps = next;
                    if let Some(rate) = rate.upgrade() {
                        set_if_present(&rate, "max-rate", &fps.to_string());
                    }
                }
                if crate::debug_logging() && tick % 2 == 0 {
                    // The byte count is the stream's, not this share's: the
                    // first line of a second share counted the first one too.
                    let kbps = bytes_sent.saturating_sub(last_bytes.unwrap_or(bytes_sent)) * 8 / 2 / 1000;
                    debug_log!(
                        "[DeskLink] screen-stats: sent {kbps} kbps, target {} kbps, fps cap {fps}, loss {:.1}%, rtt {} ms, jitter {} ms, {}",
                        bitrate / 1000,
                        report.loss * 100.0,
                        report.rtt_ms.map_or("?".to_string(), |rtt| format!("{rtt:.0}")),
                        report.jitter_ms.map_or("?".to_string(), |jitter| format!("{jitter:.0}")),
                        chain_stats.line()
                    );
                }
                if tick % 2 == 0 || last_bytes.is_none() {
                    last_bytes = Some(bytes_sent);
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("[DeskLink] Screen bitrate control unavailable: {error}");
    }
    stop
}

/// The phone's receiver reports (the worst of them) and the bytes sent so
/// far, from `webrtcbin` stats; otherwise why there are none. Waits at most
/// [`STATS_WAIT`], like the peer's own stats read: an unanswered request
/// used to hold the controller for the rest of the share.
fn link_report(
    webrtcbin: &gst::Element,
    ssrc: Option<u32>,
) -> Result<(LinkReport, u64), &'static str> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let promise = gst::Promise::with_change_func(move |reply| {
        let _ = sender.send(reply.ok().flatten().map(|stats| stats.to_owned()));
    });
    webrtcbin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    let reply = match receiver.recv_timeout(STATS_WAIT) {
        Ok(Some(reply)) => reply,
        Ok(None) => return Err("webrtcbin answered the stats request with nothing"),
        Err(_) => return Err("webrtcbin did not answer the stats request"),
    };
    let mut report = LinkReport::default();
    let mut bytes = 0u64;
    let mut any = false;
    for stats in reply
        .iter()
        .filter_map(|(_, value)| value.get::<gst::Structure>().ok())
    {
        match stats.name().as_str() {
            "remote-inbound-rtp" => {
                any = true;
                if let Ok(lost) = stats.get::<f64>("fraction-lost") {
                    report.loss = report.loss.max(lost);
                }
                if let Ok(rtt) = stats.get::<f64>("round-trip-time") {
                    let ms = rtt * 1000.0;
                    report.rtt_ms = Some(report.rtt_ms.map_or(ms, |known: f64| known.max(ms)));
                }
                if let Ok(jitter) = stats.get::<f64>("jitter") {
                    let ms = jitter * 1000.0;
                    report.jitter_ms =
                        Some(report.jitter_ms.map_or(ms, |known: f64| known.max(ms)));
                }
            }
            "outbound-rtp" => {
                bytes += stats.get::<u64>("bytes-sent").unwrap_or(0);
            }
            _ => {}
        }
    }
    if any {
        return Ok((report, bytes));
    }
    // webrtcbin finds a sender by its SSRC map, and drops the entry when the
    // sender times out between two shares; a reused `screen_ssrc` is never
    // added back, so every share after the first had no stats at all (F12).
    // rtpbin's own session still holds the source and the phone's reports.
    ssrc.and_then(|ssrc| session_report(webrtcbin, ssrc))
        .ok_or("no receiver report from the phone for the screen stream")
}

/// The phone's last report about `ssrc`, from `webrtcbin`'s rtpbin session.
fn session_report(webrtcbin: &gst::Element, ssrc: u32) -> Option<(LinkReport, u64)> {
    let rtpbin = webrtcbin.downcast_ref::<gst::Bin>()?.by_name("rtpbin")?;
    // Max-bundle: every section is session 0.
    let session =
        rtpbin.emit_by_name::<Option<gst::glib::Object>>("get-internal-session", &[&0u32])?;
    let stats = session.property::<gst::Structure>("stats");
    let sources = stats.get::<gst::glib::ValueArray>("source-stats").ok()?;
    report_for_source(
        sources
            .iter()
            .filter_map(|value| value.get::<gst::Structure>().ok()),
        ssrc,
    )
}

/// An rtpbin source-stats entry for our own `ssrc` turned into a report,
/// once the phone has sent one (`have-rb`). GStreamer 1.24 keeps that report
/// on the phone's source instead (`rb-ssrc` naming ours), so that is read
/// when ours has none.
pub(super) fn report_for_source(
    sources: impl Iterator<Item = gst::Structure>,
    ssrc: u32,
) -> Option<(LinkReport, u64)> {
    let sources: Vec<gst::Structure> = sources.collect();
    let source = sources.iter().find(|source| {
        source.get::<u32>("ssrc").ok() == Some(ssrc)
            && source.get::<bool>("internal").unwrap_or(false)
    })?;
    let about_ours = sources.iter().filter(|other| {
        !other.get::<bool>("internal").unwrap_or(true)
            && other.get::<u32>("rb-ssrc").ok() == Some(ssrc)
    });
    let rb = std::iter::once(source)
        .chain(about_ours)
        .find(|source| source.get::<bool>("have-rb").unwrap_or(false))?;
    let clock_rate = source
        .get::<i32>("clock-rate")
        .ok()
        .filter(|rate| *rate > 0)
        .unwrap_or(90_000);
    let report = LinkReport {
        loss: f64::from(rb.get::<u32>("rb-fractionlost").unwrap_or(0)) / 256.0,
        // NTP short format: 1/65536 s.
        rtt_ms: rb
            .get::<u32>("rb-round-trip")
            .ok()
            .filter(|rtt| *rtt > 0)
            .map(|rtt| f64::from(rtt) * 1000.0 / 65536.0),
        jitter_ms: rb
            .get::<u32>("rb-jitter")
            .ok()
            .map(|jitter| f64::from(jitter) * 1000.0 / f64::from(clock_rate)),
    };
    Some((report, source.get::<u64>("octets-sent").unwrap_or(0)))
}

/// Longest the rate controller waits for `webrtcbin`'s stats.
const STATS_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Debug logging only: what the encoder is handed, makes and is asked for,
/// so a choppy or frozen picture on the phone can be traced to its stage.
#[derive(Clone, Default)]
pub(super) struct ChainStats {
    /// Frames offered to the encoder's queue (more than `encoded`: the
    /// one-frame leaky queue dropped the rest).
    offered: Arc<AtomicU64>,
    encoded: Arc<AtomicU64>,
    pub(super) keyframes: Arc<AtomicU64>,
    /// `GstForceKeyUnit` requests (the phone's PLI/FIR) reaching the encoder.
    pub(super) key_requests: Arc<AtomicU64>,
}

impl ChainStats {
    pub(super) fn watch(&self, queue_sink: &Option<gst::Pad>, encoder: &gst::Element) {
        if let Some(pad) = queue_sink {
            let offered = Arc::clone(&self.offered);
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                offered.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        }
        let Some(pad) = encoder.static_pad("src") else {
            return;
        };
        let (encoded, keyframes) = (Arc::clone(&self.encoded), Arc::clone(&self.keyframes));
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                encoded.fetch_add(1, Ordering::Relaxed);
                if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                    keyframes.fetch_add(1, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        });
        let requests = Arc::clone(&self.key_requests);
        pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                if event
                    .structure()
                    .is_some_and(|s| s.name() == "GstForceKeyUnit")
                {
                    requests.fetch_add(1, Ordering::Relaxed);
                }
            }
            gst::PadProbeReturn::Ok
        });
    }

    fn line(&self) -> String {
        format!(
            "offered {} encoded {} keyframes {} key requests {}",
            self.offered.load(Ordering::Relaxed),
            self.encoded.load(Ordering::Relaxed),
            self.keyframes.load(Ordering::Relaxed),
            self.key_requests.load(Ordering::Relaxed)
        )
    }
}
