//! Sound both ways (contract F1, A1–A4), the media side.
//!
//! One Opus audio section is in every first offer and answer (no
//! renegotiation exists). Incoming audio is parked on a fakesink while idle;
//! a session builds `rtpopusdepay ! opusdec ! audioconvert ! audioresample`
//! into either the default output (the phone's sound) or a PipeWire
//! `Audio/Source` node, "DeskLink Microphone" (the phone's microphone).
//! Outgoing audio is this computer's sound (the default sink's monitor) or
//! its microphone, Opus-encoded into the negotiated audio section. All of it
//! is PipeWire through GStreamer: nothing to install.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gst::prelude::*;

/// Opus as A1 states it: 48 kHz stereo, 10 ms minimum, in-band FEC, up to
/// 128 kbit/s. `webrtcbin` writes the extra fields as the `a=fmtp` line.
pub(crate) fn audio_caps() -> gst::Caps {
    gst::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("encoding-name", "OPUS")
        .field("clock-rate", 48_000i32)
        .field("payload", DEFAULT_OPUS_PAYLOAD as i32)
        .field("encoding-params", "2")
        .field("minptime", "10")
        .field("useinbandfec", "1")
        .field("stereo", "1")
        .field("maxaveragebitrate", "128000")
        .build()
}

/// What libwebrtc uses for Opus; the negotiated one is read from the SDP.
const DEFAULT_OPUS_PAYLOAD: u32 = 111;
/// What the desk encodes at on a LAN.
const OPUS_BITRATE: i32 = 128_000;
/// The PipeWire node a phone's microphone becomes.
pub(crate) const MICROPHONE_NODE_DESCRIPTION: &str = "DeskLink Microphone";
/// A 10 ms PipeWire quantum: small buffers, low latency.
const NODE_LATENCY: &str = "480/48000";

/// Where incoming audio goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// The default output (the phone's own sound).
    Speakers,
    /// A PipeWire `Audio/Source` every app can pick ("DeskLink Microphone").
    Microphone,
}

/// What outgoing audio is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// Everything this computer plays (the default sink's monitor).
    System,
    /// This computer's default microphone.
    Mic,
}

fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|error| format!("DeskLink sound needs GStreamer's {factory}: {error}"))
}

fn output_sink(output: Output, phone: &str) -> Result<gst::Element, String> {
    if cfg!(test) {
        return gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()
            .map_err(|error| error.to_string());
    }
    let sink = make("pipewiresink")?;
    let properties = match output {
        Output::Speakers => gst::Structure::builder("properties")
            .field("media.role", "Music")
            .field("node.name", "desklink-phone-sound")
            .field("node.description", format!("Sound from {phone}"))
            .field("node.latency", NODE_LATENCY)
            .build(),
        Output::Microphone => {
            sink.set_property_from_str("mode", "provide");
            gst::Structure::builder("properties")
                .field("media.class", "Audio/Source")
                .field("node.name", "desklink-microphone")
                .field("node.description", MICROPHONE_NODE_DESCRIPTION)
                .field("node.latency", NODE_LATENCY)
                .build()
        }
    };
    sink.set_property("client-name", "DeskLink");
    sink.set_property("stream-properties", properties);
    sink.set_property("sync", false);
    // Joining a playing pipeline must not preroll: an async sink makes the
    // pipeline lose and regain PLAYING, with a new base time that left live
    // sources already running waiting on the clock for good.
    sink.set_property("async", false);
    Ok(sink)
}

/// Incoming audio being played or published.
pub(crate) struct AudioPlayback {
    pipeline: gst::Pipeline,
    pad: gst::Pad,
    elements: Vec<gst::Element>,
    received: Arc<AtomicU64>,
    pub(crate) output: Output,
    pub(crate) phone: String,
}

impl AudioPlayback {
    /// Takes the incoming audio pad off its parking fakesink and decodes it
    /// into `output`.
    pub(crate) fn start(
        pipeline: &gst::Pipeline,
        pad: &gst::Pad,
        output: Output,
        phone: &str,
    ) -> Result<Self, String> {
        super::screen::while_blocked(pad, || Self::start_blocked(pipeline, pad, output, phone))
    }

    fn start_blocked(
        pipeline: &gst::Pipeline,
        pad: &gst::Pad,
        output: Output,
        phone: &str,
    ) -> Result<Self, String> {
        unpark(pipeline, pad);
        let depay = make("rtpopusdepay")?;
        let decode = gst::ElementFactory::make("opusdec")
            .property("use-inband-fec", true)
            .property("plc", true)
            .build()
            .map_err(|error| format!("DeskLink sound needs GStreamer's opusdec: {error}"))?;
        let convert = make("audioconvert")?;
        let resample = make("audioresample")?;
        let sink = output_sink(output, phone)?;
        let elements = vec![depay, decode, convert, resample, sink];
        pipeline
            .add_many(elements.iter())
            .map_err(|error| format!("Could not build DeskLink sound: {error}"))?;
        let received = Arc::new(AtomicU64::new(0));
        let build = || -> Result<(), String> {
            gst::Element::link_many(elements.iter()).map_err(|error| error.to_string())?;
            for element in elements.iter().rev() {
                element
                    .sync_state_with_parent()
                    .map_err(|error| format!("Could not start DeskLink sound: {error}"))?;
            }
            if let Some(sink_pad) = elements.last().and_then(|sink| sink.static_pad("sink")) {
                let counter = Arc::clone(&received);
                sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                });
            }
            let target = elements[0]
                .static_pad("sink")
                .ok_or_else(|| "DeskLink sound has no input".to_string())?;
            pad.link(&target)
                .map_err(|error| format!("Could not attach the phone's sound: {error}"))?;
            Ok(())
        };
        if let Err(error) = build() {
            for element in elements.iter().rev() {
                let _ = element.set_state(gst::State::Null);
            }
            let _ = pipeline.remove_many(elements.iter());
            super::screen::park_pad(pipeline, pad);
            return Err(error);
        }
        Ok(Self {
            pipeline: pipeline.clone(),
            pad: pad.clone(),
            elements,
            received,
            output,
            phone: phone.to_string(),
        })
    }

    /// Decoded buffers that reached the output.
    pub(crate) fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Ends it; the pad goes back on its fakesink (RTP into an unlinked pad
    /// would stop webrtcbin's shared receive path).
    pub(crate) fn stop(self) {
        debug_log!(
            "[DeskLink] Phone sound ended after {} packets",
            self.received()
        );
        let pad = self.pad.clone();
        super::screen::while_blocked(&pad, || {
            if let Some(target) = self.elements.first().and_then(|e| e.static_pad("sink")) {
                let _ = self.pad.unlink(&target);
            }
            super::screen::park_pad(&self.pipeline, &self.pad);
            for element in self.elements.iter().rev() {
                let _ = element.set_state(gst::State::Null);
            }
            let _ = self.pipeline.remove_many(self.elements.iter());
        });
    }
}

/// Frees a parked pad from its fakesink.
fn unpark(pipeline: &gst::Pipeline, pad: &gst::Pad) {
    if let Some(peer) = pad.peer() {
        let _ = pad.unlink(&peer);
        if let Some(fake) = peer.parent_element() {
            let _ = fake.set_state(gst::State::Null);
            let _ = pipeline.remove(&fake);
        }
    }
}

fn source_element(source: Source) -> Result<gst::Element, String> {
    if cfg!(test) {
        return gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .property("do-timestamp", true)
            .build()
            .map_err(|error| error.to_string());
    }
    let element = make("pipewiresrc")?;
    let properties = match source {
        // The default output's monitor: what this computer plays.
        Source::System => gst::Structure::builder("properties")
            .field("stream.capture.sink", true)
            .field("node.name", "desklink-sound-to-phone")
            .field("node.description", "DeskLink: this computer's sound")
            .field("node.latency", NODE_LATENCY)
            .build(),
        Source::Mic => gst::Structure::builder("properties")
            .field("media.role", "Communication")
            .field("node.name", "desklink-mic-to-phone")
            .field("node.description", "DeskLink: microphone to the phone")
            .field("node.latency", NODE_LATENCY)
            .build(),
    };
    element.set_property("client-name", "DeskLink");
    element.set_property("stream-properties", properties);
    element.set_property("do-timestamp", true);
    Ok(element)
}

/// The sink pad of the negotiated audio section: the one `webrtcbin` made
/// for an offer, else requested by its m-line (an unnamed request would make
/// a new, never-negotiated transceiver).
fn audio_sink_pad(webrtcbin: &gst::Element) -> Option<gst::Pad> {
    let existing = webrtcbin.sink_pads().into_iter().find(|pad| {
        !pad.is_linked()
            && pad.find_property("transceiver").is_some()
            && pad
                .property::<Option<gst_webrtc::WebRTCRTPTransceiver>>("transceiver")
                .is_some_and(|transceiver| transceiver.kind() == gst_webrtc::WebRTCKind::Audio)
    });
    if existing.is_some() {
        return existing;
    }
    let mline = (0i32..16).find_map(|index| {
        let transceiver = webrtcbin.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
            "get-transceiver",
            &[&index],
        )?;
        let mline = transceiver.property::<u32>("mlineindex");
        (transceiver.kind() == gst_webrtc::WebRTCKind::Audio && mline != u32::MAX).then_some(mline)
    })?;
    webrtcbin.request_pad_simple(&format!("sink_{mline}"))
}

/// Whether this connection negotiated the audio section at all (an older
/// phone answers without it, or rejects it with port 0).
pub(crate) fn negotiated(webrtcbin: &gst::Element) -> bool {
    let Some(local) =
        webrtcbin.property::<Option<gst_webrtc::WebRTCSessionDescription>>("local-description")
    else {
        return false;
    };
    let Some(remote) =
        webrtcbin.property::<Option<gst_webrtc::WebRTCSessionDescription>>("remote-description")
    else {
        return false;
    };
    live(local.sdp()) && live(remote.sdp())
}

/// Whether `sdp` has an audio section that is not rejected or inactive. A
/// max-bundle offer gives bundled sections port 0 with `bundle-only`; a
/// rejected one has port 0 and is out of the BUNDLE group.
fn live(sdp: &gst_sdp::SDPMessageRef) -> bool {
    let bundled: Vec<String> = sdp
        .attributes()
        .filter(|attribute| attribute.key() == "group")
        .filter_map(|attribute| attribute.value().map(str::to_string))
        .filter(|value| value.starts_with("BUNDLE"))
        .flat_map(|value| {
            value
                .split_whitespace()
                .skip(1)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    sdp.medias().any(|media| {
        let mid = media.attribute_val("mid").unwrap_or_default();
        media.media() == Some("audio")
            && media.attribute_val("inactive").is_none()
            && media
                .attributes()
                .all(|attribute| attribute.key() != "inactive")
            && (media.port() != 0 || bundled.iter().any(|known| known == mid))
    })
}

/// This computer's sound or microphone, sent to the phone.
pub(crate) struct AudioSend {
    pipeline: gst::Pipeline,
    sink_pad: gst::Pad,
    elements: Vec<gst::Element>,
    sent: Arc<AtomicU64>,
    pub(crate) source: Source,
}

impl AudioSend {
    pub(crate) fn start(
        pipeline: &gst::Pipeline,
        webrtcbin: &gst::Element,
        source: Source,
        continuity: Continuity,
    ) -> Result<Self, String> {
        Self::start_from(
            pipeline,
            webrtcbin,
            source,
            source_element(source)?,
            continuity,
        )
    }

    /// Everything after the source, so tests can feed `audiotestsrc`.
    pub(crate) fn start_from(
        pipeline: &gst::Pipeline,
        webrtcbin: &gst::Element,
        source: Source,
        input: gst::Element,
        continuity: Continuity,
    ) -> Result<Self, String> {
        if !negotiated(webrtcbin) {
            return Err("The phone's DeskLink cannot play sound yet (update it)".to_string());
        }
        let sink_pad = audio_sink_pad(webrtcbin)
            .ok_or_else(|| "This connection has no sound track".to_string())?;
        // webrtcbin paces each input with a `clocksync`. Sound is pushed as
        // it is captured (stamped with the running time), so waiting there
        // only adds latency; a session started on a running connection
        // also stalled in that wait after two packets.
        if let Some(bin) = webrtcbin.downcast_ref::<gst::Bin>() {
            for element in bin.iterate_recurse().into_iter().flatten() {
                if element
                    .factory()
                    .is_some_and(|factory| factory.name() == "clocksync")
                    && element.find_property("sync").is_some()
                    && element.property::<bool>("sync")
                {
                    element.set_property("sync", false);
                }
            }
        }
        let convert = make("audioconvert")?;
        let resample = make("audioresample")?;
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("audio/x-raw")
                    .field("rate", 48_000i32)
                    .field("channels", 2i32)
                    .build(),
            )
            .build()
            .map_err(|error| error.to_string())?;
        let encoder = make("opusenc")?;
        encoder.set_property("bitrate", OPUS_BITRATE);
        encoder.set_property("inband-fec", true);
        encoder.set_property("packet-loss-percentage", 5i32);
        encoder.set_property_from_str("frame-size", "10");
        encoder.set_property_from_str(
            "audio-type",
            if source == Source::Mic {
                "voice"
            } else {
                "generic"
            },
        );
        let payloader = make("rtpopuspay")?;
        let payload_type = super::screen::negotiated_payload_in(webrtcbin, "audio", "OPUS")
            .unwrap_or(DEFAULT_OPUS_PAYLOAD);
        payloader.set_property("pt", payload_type);
        // One SSRC for every session on this connection, its sequence
        // carried on: a new SSRC is a new receive pad on the far side (silence
        // until it is found), and a reset sequence trips SRTP's replay check.
        payloader.set_property("ssrc", continuity.ssrc);
        // One RTP clock for the connection too: the payloader's timestamps
        // are this offset plus the running time, so a new session carries on
        // where the last one's time would be, instead of jumping (a jump the
        // far side's jitter buffer sat out).
        payloader.set_property("timestamp-offset", continuity.timestamp_offset);
        if let Some(next) = continuity.next_seqnum {
            payloader.set_property("seqnum-offset", i32::from(next));
        }
        let elements = vec![input, convert, resample, caps, encoder, payloader];
        pipeline
            .add_many(elements.iter())
            .map_err(|error| format!("Could not build DeskLink sound: {error}"))?;
        let sent = Arc::new(AtomicU64::new(0));
        let build = || -> Result<(), String> {
            gst::Element::link_many(elements.iter()).map_err(|error| error.to_string())?;
            let src = elements
                .last()
                .and_then(|payloader| payloader.static_pad("src"))
                .ok_or_else(|| "DeskLink sound has no output".to_string())?;
            let counter = Arc::clone(&sent);
            src.add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |_, info| {
                    let packets = match &info.data {
                        Some(gst::PadProbeData::Buffer(_)) => 1,
                        Some(gst::PadProbeData::BufferList(list)) => list.len() as u64,
                        _ => 0,
                    };
                    counter.fetch_add(packets, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                },
            );
            // Never EOS into webrtcbin's shared sink: it would end the track
            // for good. Sessions end by being unlinked.
            src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_, info| {
                match &info.data {
                    Some(gst::PadProbeData::Event(event))
                        if event.type_() == gst::EventType::Eos =>
                    {
                        gst::PadProbeReturn::Drop
                    }
                    _ => gst::PadProbeReturn::Ok,
                }
            });
            src.link(&sink_pad)
                .map_err(|error| format!("Could not attach sound to the call: {error}"))?;
            for element in elements.iter().rev() {
                element
                    .sync_state_with_parent()
                    .map_err(|error| format!("Could not start DeskLink sound: {error}"))?;
            }
            Ok(())
        };
        if let Err(error) = build() {
            for element in elements.iter().rev() {
                let _ = element.set_state(gst::State::Null);
            }
            let _ = pipeline.remove_many(elements.iter());
            return Err(error);
        }
        debug_log!(
            "[DeskLink] Sound to the phone on {} pt {payload_type}",
            sink_pad.name()
        );
        Ok(Self {
            pipeline: pipeline.clone(),
            sink_pad,
            elements,
            sent,
            source,
        })
    }

    /// RTP packets sent so far.
    pub(crate) fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Ends it: stopped first (no EOS goes out), then unlinked. The sink
    /// pad is kept for the next session. Returns where the next session's
    /// sequence continues.
    pub(crate) fn stop(self) -> Option<u16> {
        let next = self
            .elements
            .last()
            .filter(|_| self.sent() > 0)
            .map(|payloader| payloader.property::<u32>("seqnum"))
            .map(|seqnum| (seqnum as u16).wrapping_add(1));
        debug_log!(
            "[DeskLink] Sound to the phone ended after {} packets",
            self.sent()
        );
        for element in &self.elements {
            let _ = element.set_state(gst::State::Null);
        }
        if let Some(src) = self.elements.last().and_then(|e| e.static_pad("src")) {
            let _ = src.unlink(&self.sink_pad);
        }
        let _ = self.pipeline.remove_many(self.elements.iter());
        next
    }
}

/// What keeps a connection's sound one RTP stream across sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Continuity {
    pub(crate) ssrc: u32,
    pub(crate) timestamp_offset: u32,
    pub(crate) next_seqnum: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audio_section_states_opus_as_a1_says() {
        gst::init().unwrap();
        let caps = audio_caps();
        let structure = caps.structure(0).unwrap();
        assert_eq!(structure.get::<&str>("encoding-name").unwrap(), "OPUS");
        assert_eq!(structure.get::<i32>("clock-rate").unwrap(), 48_000);
        for (key, value) in [
            ("minptime", "10"),
            ("useinbandfec", "1"),
            ("stereo", "1"),
            ("maxaveragebitrate", "128000"),
        ] {
            assert_eq!(structure.get::<&str>(key).unwrap(), value, "{key}");
        }
    }
}
