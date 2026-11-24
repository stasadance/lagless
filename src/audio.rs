//! Opus audio m-lines on the same peer as the data channels (BUNDLE, one DTLS session).
//!
//! The server never encodes: it answers the browser's audio m-lines with Opus at the
//! payload type the browser offered, receives RTP on the browser's sendonly m-lines and
//! sends finished RTP packets from the app on the browser's recvonly ones. rtc does SRTP,
//! SRTCP and the RTCP sender/receiver reports.

use crate::AudioOptions;
use anyhow::{anyhow, bail};
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::RTCPeerConnection;
use rtc::peer_connection::configuration::interceptor_registry::configure_rtcp_reports;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use rtc::rtp_transceiver::{RTCRtpSenderId, RTCRtpTransceiverDirection};
use std::collections::HashMap;

pub const DEFAULT_FMTP: &str = "minptime=10;useinbandfec=1";
const DEFAULT_STREAM_ID: &str = "lagless";

pub struct Send {
    pub mid: String,
    pub ssrc: u32,
    stream_id: String,
    track_id: String,
}

pub struct Plan {
    pub pt: u8,
    fmtp: String,
    /// every audio m-line with Opus, in offer order
    mids: Vec<String>,
    pub recv: Vec<String>,
    pub send: Vec<Send>,
}

pub struct Wired {
    pub senders: HashMap<String, RTCRtpSenderId>,
    /// audio mids that carry RTP either way, in offer order (rtpopen events)
    pub mids: Vec<String>,
}

/// What the browser offers, as (mid, direction, opus pt) per audio m-line.
fn audio_lines(
    offer: &RTCSessionDescription,
) -> anyhow::Result<Vec<(String, &'static str, Option<u8>)>> {
    let parsed = offer.unmarshal()?;
    let mut out = Vec::new();
    for m in &parsed.media_descriptions {
        // port 0 is a rejected m-line, unless bundle-only (Firefox's BUNDLE offers)
        let rejected = m.media_name.port.value == 0 && m.attribute("bundle-only").is_none();
        if m.media_name.media != "audio" || rejected {
            continue;
        }
        let Some(Some(mid)) = m.attribute("mid") else {
            continue;
        };
        let dir = ["sendrecv", "sendonly", "recvonly", "inactive"]
            .into_iter()
            .find(|d| m.attribute(d).is_some())
            .unwrap_or("sendrecv");
        let pt = m.attributes.iter().find_map(|a| {
            if a.key != "rtpmap" {
                return None;
            }
            let v = a.value.as_deref()?;
            let (pt, codec) = v.split_once(' ')?;
            codec
                .to_ascii_lowercase()
                .starts_with("opus/48000")
                .then(|| pt.trim().parse::<u8>().ok())
                .flatten()
        });
        out.push((mid.to_string(), dir, pt));
    }
    Ok(out)
}

/// Decide which offered audio m-lines receive and which send. None = the offer has no
/// Opus audio (its audio m-lines, if any, are rejected as before).
pub fn plan(offer: &RTCSessionDescription, opts: &AudioOptions) -> anyhow::Result<Option<Plan>> {
    let lines = audio_lines(offer)?;
    let Some(pt) = lines.iter().find_map(|l| l.2) else {
        return Ok(None);
    };
    let lines: Vec<_> = lines.into_iter().filter(|l| l.2.is_some()).collect();
    let recv = lines
        .iter()
        .filter(|l| l.1 == "sendonly" || l.1 == "sendrecv")
        .map(|l| l.0.clone())
        .collect();
    let mut send: Vec<Send> = Vec::new();
    let wanted = opts.send.clone().unwrap_or_default();
    // explicit mids first, so an auto slot never takes one that is named later
    let named: Vec<&str> = wanted.iter().filter_map(|w| w.mid.as_deref()).collect();
    for (i, w) in wanted.iter().enumerate() {
        let mid = match &w.mid {
            Some(mid) => {
                let line = lines.iter().find(|l| &l.0 == mid).ok_or_else(|| {
                    anyhow!("audio send[{i}]: no Opus audio m-line with mid {mid}")
                })?;
                if line.1 != "recvonly" && line.1 != "sendrecv" {
                    bail!(
                        "audio send[{i}]: mid {mid} is {} in the offer, the browser does not receive on it",
                        line.1
                    );
                }
                mid.clone()
            }
            None => lines
                .iter()
                .find(|l| {
                    l.1 == "recvonly"
                        && !named.contains(&l.0.as_str())
                        && !send.iter().any(|s| s.mid == l.0)
                })
                .map(|l| l.0.clone())
                .ok_or_else(|| {
                    anyhow!("audio send[{i}]: no free recvonly audio m-line in the offer")
                })?,
        };
        if send.iter().any(|s| s.mid == mid) {
            bail!("audio send[{i}]: mid {mid} is used twice");
        }
        if send.iter().any(|s| s.ssrc == w.ssrc) {
            bail!("audio send[{i}]: ssrc {} is used twice", w.ssrc);
        }
        send.push(Send {
            mid,
            ssrc: w.ssrc,
            stream_id: w
                .stream_id
                .clone()
                .unwrap_or_else(|| DEFAULT_STREAM_ID.into()),
            track_id: w.track_id.clone().unwrap_or_else(|| format!("audio{i}")),
        });
    }
    Ok(Some(Plan {
        pt,
        fmtp: opts.fmtp.clone().unwrap_or_else(|| DEFAULT_FMTP.into()),
        mids: lines.into_iter().map(|l| l.0).collect(),
        recv,
        send,
    }))
}

fn opus(plan: &Plan) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: plan.fmtp.clone(),
            rtcp_feedback: vec![],
        },
        payload_type: plan.pt,
    }
}

/// Opus only, at the browser's payload type; RTCP sender + receiver reports, nothing else
/// (no NACK/TWCC: Opus does not need them).
pub fn engine(plan: &Plan) -> anyhow::Result<(MediaEngine, Registry)> {
    let mut me = MediaEngine::default();
    me.register_codec(opus(plan), RtpCodecKind::Audio)?;
    Ok((me, configure_rtcp_reports(Registry::new())))
}

/// After set_remote_description: one sender per send slot,
/// browser-recvonly m-lines without a slot answered inactive.
pub fn attach(pc: &mut RTCPeerConnection, plan: &Plan) -> anyhow::Result<Wired> {
    let codec = opus(plan);
    let mut by_mid = HashMap::new();
    for t in pc.get_transceivers().collect::<Vec<_>>() {
        let Some(tr) = pc.rtp_transceiver(t) else {
            continue;
        };
        let Some(mid) = tr.mid().clone() else {
            continue;
        };
        if plan.mids.contains(&mid) {
            by_mid.insert(mid, t);
        }
    }
    let mut senders = HashMap::new();
    for s in &plan.send {
        let t = *by_mid
            .get(&s.mid)
            .ok_or_else(|| anyhow!("no transceiver for audio mid {}", s.mid))?;
        let track = MediaStreamTrack::new(
            s.stream_id.clone(),
            s.track_id.clone(),
            s.track_id.clone(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(s.ssrc),
                    ..Default::default()
                },
                codec: codec.rtp_codec.clone(),
                ..Default::default()
            }],
        );
        senders.insert(s.mid.clone(), pc.add_track_to_transceiver(t, track)?);
    }
    for (mid, &t) in &by_mid {
        if senders.contains_key(mid) || plan.recv.contains(mid) {
            continue;
        }
        if let Some(mut tr) = pc.rtp_transceiver(t) {
            tr.set_direction(RTCRtpTransceiverDirection::Inactive);
        }
    }
    let mids = plan
        .mids
        .iter()
        .filter(|m| senders.contains_key(*m) || plan.recv.contains(m))
        .cloned()
        .collect();
    Ok(Wired { senders, mids })
}

/// Put our Opus fmtp in the answer's audio m-lines. rtc answers with the offer's fmtp;
/// ours tells the browser how to encode (stereo, bitrate).
pub fn set_fmtp(sdp: &str, plan: &Plan) -> String {
    let rtpmap = format!("a=rtpmap:{} ", plan.pt);
    let fmtp = format!("a=fmtp:{} ", plan.pt);
    let mut out = String::with_capacity(sdp.len() + 256);
    let mut audio = false;
    let mut pending = false;
    let line = format!("a=fmtp:{} {}", plan.pt, plan.fmtp);
    for l in sdp.split_inclusive("\r\n") {
        let t = l.trim_end();
        if t.starts_with("m=") {
            if pending {
                out.push_str(&line);
                out.push_str("\r\n");
            }
            pending = false;
            audio = t.starts_with("m=audio ") && !t.starts_with("m=audio 0 ");
        }
        if audio && t.starts_with(&fmtp) {
            if pending {
                out.push_str(&line);
                out.push_str("\r\n");
                pending = false;
            }
            continue;
        }
        if audio && pending && !t.starts_with("a=rtcp-fb:") {
            out.push_str(&line);
            out.push_str("\r\n");
            pending = false;
        }
        out.push_str(l);
        if audio && t.starts_with(&rtpmap) {
            pending = true;
        }
    }
    if pending {
        out.push_str(&line);
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmtp_replaced_or_added_on_audio_only() {
        let plan = Plan {
            pt: 111,
            fmtp: "stereo=0".into(),
            mids: vec![],
            recv: vec![],
            send: vec![],
        };
        let sdp = "v=0\r\nm=audio 9 X 111\r\na=rtpmap:111 opus/48000/2\r\na=fmtp:111 minptime=10\r\na=sendonly\r\n\
m=audio 9 X 111\r\na=rtpmap:111 opus/48000/2\r\na=recvonly\r\n\
m=audio 0 X 0\r\na=mid:9\r\nm=application 9 Y\r\na=fmtp:111 keep\r\n";
        let out = set_fmtp(sdp, &plan);
        assert_eq!(out.matches("a=fmtp:111 stereo=0\r\n").count(), 2);
        assert!(!out.contains("minptime"));
        assert!(out.contains("a=fmtp:111 keep\r\n"));
        assert!(out.contains("a=rtpmap:111 opus/48000/2\r\na=fmtp:111 stereo=0\r\na=recvonly"));
    }
}
