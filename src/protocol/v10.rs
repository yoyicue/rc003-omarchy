//! ATVV v1.0 protocol implementation.

use anyhow::Result;

use super::types::*;
use super::Protocol;
use crate::adpcm::AdpcmDecoder;

// v1.0 CTL payload minimum lengths (including opcode byte).
const AUDIO_START_MIN_LEN: usize = 4; // opcode + reason(1) + codec(1) + stream_id(1)
const AUDIO_SYNC_MIN_LEN: usize = 7; // opcode + codec(1) + frame_num(2) + predictor(2) + step_index(1)
const CAPS_RESP_MIN_LEN: usize = 7; // opcode + version(2) + codecs(1) + model(1) + frame_size(2)
const MIC_OPEN_ERROR_MIN_LEN: usize = 3; // opcode + error_code(2)

/// Parse a v1.0 CAPS_RESP payload. Returns None if data is too short.
///
/// v1.0 layout: [opcode(1), version(2), codecs(1), interaction_model(1), frame_size(2)]
pub(crate) fn parse_caps_resp_payload(data: &[u8]) -> Option<Capabilities> {
    if data.len() < CAPS_RESP_MIN_LEN {
        return None;
    }
    let version_wire = u16::from_be_bytes([data[1], data[2]]);
    // Defensive default: if the wire version is unrecognized, assume v1.0
    // for forward-compatibility with future protocol versions.
    let version = ProtocolVersion::from_wire(version_wire).unwrap_or(ProtocolVersion::V1_0);
    let codecs = Codecs::from_bits_truncate(data[3]);
    let interaction_model =
        InteractionModel::try_from(data[4]).unwrap_or(InteractionModel::OnRequest);
    let frame_size = u16::from_be_bytes([data[5], data[6]]);
    Some(Capabilities {
        version,
        codecs,
        interaction_model,
        audio_frame_size: AudioFrameSize(frame_size),
    })
}

/// ATVV v1.0 protocol implementation.
///
/// Supports Push-to-Talk (PTT) and Hold-to-Talk (HTT) interaction models
/// in addition to the v0.4 OnRequest model. RC003 uses headerless frames by
/// default; firmware with a six-byte sync header requires explicit configuration.
pub struct ProtocolV10 {
    pub(crate) selected_codec: Option<Codec>,
    pub(crate) interaction_model: InteractionModel,
    pub(crate) audio_frame_size: AudioFrameSize,
    pub(crate) decoder: AdpcmDecoder,
    pub(crate) frame_seq: u16,
    audio_buffer: Vec<u8>,
    header_mode: bool,
    awaiting_sync: bool,
}

impl Default for ProtocolV10 {
    fn default() -> Self {
        Self {
            selected_codec: None,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize::DEFAULT_V10,
            decoder: AdpcmDecoder::new(),
            frame_seq: 0,
            audio_buffer: Vec::new(),
            header_mode: false,
            awaiting_sync: false,
        }
    }
}

impl ProtocolV10 {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Protocol for ProtocolV10 {
    fn version(&self) -> ProtocolVersion {
        ProtocolVersion::V1_0
    }

    fn mic_open_cmd(&self) -> Vec<u8> {
        vec![u8::from(TxOpcode::MicOpen), u8::from(AudioMode::Playback)]
    }

    fn mic_close_cmd(&self, stream_id: StreamId) -> Vec<u8> {
        vec![u8::from(TxOpcode::MicClose), stream_id.0]
    }

    fn keepalive_cmd(&self, stream_id: StreamId) -> Vec<u8> {
        vec![u8::from(TxOpcode::MicExtend), stream_id.0]
    }

    fn parse_ctl(&self, data: &[u8]) -> CtlEvent {
        if data.is_empty() {
            return CtlEvent::Unknown(data.to_vec());
        }
        let opcode = match CtlOpcode::try_from(data[0]) {
            Ok(op) => op,
            Err(_) => return CtlEvent::Unknown(data.to_vec()),
        };
        match opcode {
            CtlOpcode::AudioStop => {
                // v1.0: [opcode, reason(1)]
                let reason = if data.len() >= 2 {
                    AudioStopReason::from(data[1])
                } else {
                    AudioStopReason::MicClose
                };
                CtlEvent::AudioStop { reason }
            }
            CtlOpcode::AudioStart => {
                // v1.0: [opcode, reason(1), codec(1), stream_id(1)]
                if data.len() >= AUDIO_START_MIN_LEN {
                    let reason =
                        AudioStartReason::try_from(data[1]).unwrap_or(AudioStartReason::MicOpen);
                    let codec = Codec::try_from(data[2]).unwrap_or(Codec::Adpcm8kHz);
                    let stream_id = StreamId(data[3]);
                    CtlEvent::AudioStart {
                        reason,
                        codec,
                        stream_id,
                    }
                } else {
                    CtlEvent::Unknown(data.to_vec())
                }
            }
            CtlOpcode::StartSearch => CtlEvent::StartSearch,
            CtlOpcode::AudioSync => {
                // v1.0: [opcode, codec(1), frame_num(2), predictor(2), step_index(1)]
                if data.len() >= AUDIO_SYNC_MIN_LEN {
                    let codec = Codec::try_from(data[1]).unwrap_or(Codec::Adpcm8kHz);
                    let seq = u16::from_be_bytes([data[2], data[3]]);
                    let predictor = i16::from_be_bytes([data[4], data[5]]);
                    let step_index = data[6];
                    CtlEvent::AudioSync(AudioSyncData::Full {
                        codec,
                        seq,
                        predictor,
                        step_index,
                    })
                } else {
                    CtlEvent::Unknown(data.to_vec())
                }
            }
            CtlOpcode::CapsResp => match parse_caps_resp_payload(data) {
                Some(caps) => CtlEvent::CapsResp(caps),
                None => CtlEvent::Unknown(data.to_vec()),
            },
            CtlOpcode::MicOpenError => {
                if data.len() >= MIC_OPEN_ERROR_MIN_LEN {
                    let code = u16::from_be_bytes([data[1], data[2]]);
                    CtlEvent::MicOpenError(MicOpenErrorCode::from(code))
                } else {
                    CtlEvent::Unknown(data.to_vec())
                }
            }
        }
    }

    fn on_caps_resp(&mut self, caps: &Capabilities) -> Result<Codec> {
        let codec = super::negotiate_codec(caps.codecs)?;
        self.selected_codec = Some(codec);
        self.interaction_model = caps.interaction_model;
        self.audio_frame_size = caps.audio_frame_size;
        self.audio_buffer.clear();
        self.awaiting_sync = false;
        Ok(codec)
    }

    fn negotiated_codec(&self) -> Option<Codec> {
        self.selected_codec
    }

    fn decode_audio(&mut self, data: &[u8]) -> Vec<AudioFrame> {
        let Some(codec) = self.selected_codec else {
            return Vec::new();
        };
        if self.awaiting_sync {
            return Vec::new();
        }
        self.audio_buffer.extend_from_slice(data);

        let plain_size = self.audio_frame_size.0 as usize;
        if plain_size == 0 {
            self.audio_buffer.clear();
            return Vec::new();
        }
        let wire_size = if self.header_mode {
            plain_size + 6
        } else {
            plain_size
        };
        let mut frames = Vec::new();
        while self.audio_buffer.len() >= wire_size {
            let raw: Vec<u8> = self.audio_buffer.drain(..wire_size).collect();
            let payload = if self.header_mode {
                let predictor = i16::from_be_bytes([raw[3], raw[4]]);
                self.decoder.reset(predictor, raw[5]);
                &raw[6..]
            } else {
                &raw[..]
            };
            let samples = self.decoder.decode_bytes(payload);
            let seq = self.frame_seq;
            self.frame_seq = self.frame_seq.wrapping_add(1);
            frames.push(AudioFrame {
                seq,
                codec,
                samples,
                generation: 0,
            });
        }
        frames
    }

    fn set_frame_layout(&mut self, layout: AudioFrameLayout) {
        self.header_mode = layout == AudioFrameLayout::Headered;
        self.audio_buffer.clear();
        self.awaiting_sync = false;
    }

    fn has_partial_audio(&self) -> bool {
        !self.audio_buffer.is_empty()
    }

    fn on_audio_discontinuity(&mut self) {
        self.audio_buffer.clear();
        if self.header_mode {
            // In headered mode, packet length alone cannot prove alignment after a
            // drop (e.g., losing a 6-byte header leaves 120 old payload + 6 new
            // header = 126 bytes). Wait for an explicit AUDIO_SYNC or AUDIO_START.
            self.awaiting_sync = true;
        }
    }

    fn on_audio_start(&mut self) {
        self.audio_buffer.clear();
        self.awaiting_sync = false;
        self.frame_seq = 0;
        self.decoder.reset(0, 0);
    }

    fn on_audio_sync(&mut self, sync: &AudioSyncData) {
        match sync {
            AudioSyncData::Full {
                codec,
                seq,
                predictor,
                step_index,
            } => {
                if let Some(expected) = self.selected_codec {
                    if codec.sample_rate() != expected.sample_rate() {
                        tracing::warn!(
                            actual = ?codec,
                            actual_hz = codec.sample_rate(),
                            expected = ?expected,
                            expected_hz = expected.sample_rate(),
                            "Ignoring v1.0 AUDIO_SYNC with mismatched sample rate"
                        );
                        return;
                    }
                } else {
                    self.selected_codec = Some(*codec);
                }
                self.audio_buffer.clear();
                self.awaiting_sync = false;
                self.decoder.reset(*predictor, *step_index);
                self.frame_seq = *seq;
            }
            AudioSyncData::FrameNum { seq } => {
                self.audio_buffer.clear();
                self.awaiting_sync = false;
                // Unexpected in v1.0 (v1.0 uses Full sync with decoder state),
                // but handle gracefully by updating frame sequence only.
                tracing::trace!(
                    seq,
                    "v1.0 AUDIO_SYNC: FrameNum-only (unexpected, v1.0 normally uses Full)"
                );
                self.frame_seq = *seq;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Command encoding tests ──────────────────────────────────────

    #[test]
    fn test_mic_open_cmd() {
        let p = ProtocolV10::new();
        // v1.0 MIC_OPEN: [0x0C, audio_mode]
        assert_eq!(p.mic_open_cmd(), vec![0x0C, 0x00]); // Playback mode
    }

    #[test]
    fn test_mic_close_cmd_with_stream_id() {
        let p = ProtocolV10::new();
        assert_eq!(p.mic_close_cmd(StreamId::MIC_OPEN), vec![0x0D, 0x00]);
        assert_eq!(p.mic_close_cmd(StreamId(0x05)), vec![0x0D, 0x05]);
        assert_eq!(p.mic_close_cmd(StreamId::ANY), vec![0x0D, 0xFF]);
    }

    #[test]
    fn test_keepalive_is_mic_extend() {
        let p = ProtocolV10::new();
        assert_eq!(p.keepalive_cmd(StreamId::MIC_OPEN), vec![0x0E, 0x00]);
        assert_eq!(p.keepalive_cmd(StreamId(0x03)), vec![0x0E, 0x03]);
    }

    // ── CTL parsing tests ───────────────────────────────────────────

    #[test]
    fn test_parse_audio_start_mic_open() {
        let p = ProtocolV10::new();
        // AUDIO_START: [0x04, reason=0x00, codec=0x01, stream_id=0x00]
        match p.parse_ctl(&[0x04, 0x00, 0x01, 0x00]) {
            CtlEvent::AudioStart {
                reason,
                codec,
                stream_id,
            } => {
                assert_eq!(reason, AudioStartReason::MicOpen);
                assert_eq!(codec, Codec::Adpcm8kHz);
                assert_eq!(stream_id, StreamId::MIC_OPEN);
            }
            other => panic!("expected AudioStart, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_audio_start_ptt() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0x04, 0x01, 0x02, 0x01]) {
            CtlEvent::AudioStart {
                reason,
                codec,
                stream_id,
            } => {
                assert_eq!(reason, AudioStartReason::PressToTalk);
                assert_eq!(codec, Codec::Adpcm16kHz);
                assert_eq!(stream_id, StreamId(0x01));
            }
            other => panic!("expected AudioStart PTT, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_audio_start_htt() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0x04, 0x03, 0x01, 0x02]) {
            CtlEvent::AudioStart {
                reason,
                codec: _,
                stream_id,
            } => {
                assert_eq!(reason, AudioStartReason::HoldToTalk);
                assert_eq!(stream_id, StreamId(0x02));
            }
            other => panic!("expected AudioStart HTT, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_audio_stop_with_reason() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0x00, 0x02]) {
            CtlEvent::AudioStop { reason } => {
                assert!(matches!(reason, AudioStopReason::HttButtonRelease));
            }
            other => panic!("expected AudioStop, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_audio_sync_full() {
        let p = ProtocolV10::new();
        // [0x0A, codec=0x01, frame_hi, frame_lo, pred_hi, pred_lo, step_index]
        match p.parse_ctl(&[0x0A, 0x01, 0x00, 0x0A, 0x03, 0xE8, 0x20]) {
            CtlEvent::AudioSync(AudioSyncData::Full {
                codec,
                seq,
                predictor,
                step_index,
            }) => {
                assert_eq!(codec, Codec::Adpcm8kHz);
                assert_eq!(seq, 10);
                assert_eq!(predictor, 1000); // 0x03E8
                assert_eq!(step_index, 32);
            }
            other => panic!("expected AudioSync::Full, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_caps_resp_v10() {
        let p = ProtocolV10::new();
        // [0x0B, version(2), codecs(1), interaction_model(1), frame_size(2), extra_config(1), reserved(1)]
        let data = [0x0B, 0x01, 0x00, 0x03, 0x03, 0x00, 0xA0, 0x01, 0x00];
        match p.parse_ctl(&data) {
            CtlEvent::CapsResp(caps) => {
                assert_eq!(caps.version, ProtocolVersion::V1_0);
                assert_eq!(caps.codecs, Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ);
                assert_eq!(caps.interaction_model, InteractionModel::HoldToTalk);
                assert_eq!(caps.audio_frame_size.0, 160);
            }
            other => panic!("expected CapsResp, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_mic_open_error_ptt_in_progress() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0x0C, 0x0F, 0x80]) {
            CtlEvent::MicOpenError(code) => {
                assert!(matches!(code, MicOpenErrorCode::PttHttInProgress));
            }
            other => panic!("expected MicOpenError, got {:?}", other),
        }
    }

    // ── Audio sync and frame decoding tests ──────────────

    #[test]
    fn test_on_audio_sync_resets_decoder() {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        p.audio_frame_size = AudioFrameSize(20);
        let sync = AudioSyncData::Full {
            codec: Codec::Adpcm8kHz,
            seq: 42,
            predictor: 500,
            step_index: 30,
        };
        p.on_audio_sync(&sync);
        assert_eq!(p.decoder.predictor(), 500);
        assert_eq!(p.decoder.step_index(), 30);
        assert_eq!(p.frame_seq, 42);
    }

    #[test]
    fn test_decode_headerless_frame() {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        p.audio_frame_size = AudioFrameSize(20);
        p.decoder.reset(0, 0);
        p.frame_seq = 0;

        let result = p.decode_audio(&[0u8; 20]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].seq, 0);
        assert_eq!(result[0].samples.len(), 40);
        assert_eq!(p.frame_seq, 1);
    }

    #[test]
    fn test_rc003_caps_and_120_byte_frame() {
        let data = [0x0B, 0x01, 0x00, 0x02, 0x03, 0x00, 0x78, 0x00, 0x00];
        let caps = parse_caps_resp_payload(&data).unwrap();
        assert_eq!(caps.audio_frame_size, AudioFrameSize(120));

        let mut p = ProtocolV10::new();
        assert_eq!(p.on_caps_resp(&caps).unwrap(), Codec::Adpcm16kHz);
        assert_eq!(p.decode_audio(&[0u8; 120]).len(), 1);
        let frames = p.decode_audio(&[0u8; 120]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].samples.len(), 240);
        assert_eq!(frames[0].codec.sample_rate(), 16000);
    }

    #[test]
    fn test_rc003_fragmented_and_coalesced_frames() {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm16kHz);
        p.audio_frame_size = AudioFrameSize(120);
        assert!(p.decode_audio(&[0u8; 60]).is_empty());
        assert_eq!(p.decode_audio(&[0u8; 60]).len(), 1);
        let frames = p.decode_audio(&[0u8; 240]);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].seq, 1);
        assert_eq!(frames[1].seq, 2);
    }

    fn headered_protocol() -> ProtocolV10 {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm16kHz);
        p.audio_frame_size = AudioFrameSize(120);
        p.set_frame_layout(AudioFrameLayout::Headered);
        p
    }

    fn headered_frame() -> [u8; 126] {
        let mut data = [0u8; 126];
        data[3] = 0x03;
        data[4] = 0xE8;
        data[5] = 32;
        data
    }

    fn assert_headered_samples(samples: &[i16]) {
        let mut expected = AdpcmDecoder::new();
        expected.reset(1000, 32);
        assert_eq!(samples, expected.decode_bytes(&[0u8; 120]));
    }

    #[test]
    fn test_rc003_headered_126_byte_frame_resets_decoder() {
        let frames = headered_protocol().decode_audio(&headered_frame());
        assert_eq!(frames.len(), 1);
        assert_headered_samples(&frames[0].samples);
    }

    #[test]
    fn test_rc003_headered_frame_split_across_notifications() {
        let mut p = headered_protocol();
        let data = headered_frame();
        assert!(p.decode_audio(&data[..60]).is_empty());
        let frames = p.decode_audio(&data[60..]);
        assert_eq!(frames.len(), 1);
        assert_headered_samples(&frames[0].samples);
    }

    #[test]
    fn test_rc003_headered_frame_split_as_120_plus_6() {
        let mut p = headered_protocol();
        let data = headered_frame();
        assert!(p.decode_audio(&data[..120]).is_empty());
        let frames = p.decode_audio(&data[120..]);
        assert_eq!(frames.len(), 1);
        assert_headered_samples(&frames[0].samples);
    }

    #[test]
    fn test_audio_start_preserves_layout_and_clears_decoder_state() {
        let mut p = headered_protocol();
        p.audio_buffer.extend_from_slice(&[1, 2, 3]);
        p.frame_seq = 42;
        p.decoder.reset(1000, 32);
        p.on_audio_start();
        assert!(p.audio_buffer.is_empty());
        assert!(p.header_mode);
        assert_eq!(p.frame_seq, 0);
        assert_eq!(p.decoder.predictor(), 0);
        assert_eq!(p.decoder.step_index(), 0);
    }

    #[test]
    fn test_rc003_indeterminate_fragments_are_retained() {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm16kHz);
        p.audio_frame_size = AudioFrameSize(120);
        assert!(p.decode_audio(&[0u8; 80]).is_empty());
        assert_eq!(p.decode_audio(&[0u8; 80]).len(), 1);
        assert_eq!(p.decode_audio(&[0u8; 80]).len(), 1);
        assert!(p.audio_buffer.is_empty());
    }

    #[test]
    fn test_raw_decoding_is_independent_of_notification_boundaries() {
        let data = [7u8; 240];
        let decode = |chunks: &[usize]| {
            let mut p = ProtocolV10::new();
            p.selected_codec = Some(Codec::Adpcm16kHz);
            p.audio_frame_size = AudioFrameSize(120);
            let mut offset = 0;
            let mut samples = Vec::new();
            for chunk in chunks {
                for frame in p.decode_audio(&data[offset..offset + chunk]) {
                    samples.extend(frame.samples);
                }
                offset += chunk;
            }
            samples
        };
        assert_eq!(decode(&[120, 120]).len(), 480);
        assert_eq!(decode(&[120, 120]), decode(&[126, 114]));
    }

    #[test]
    fn test_raw_one_byte_fragment_does_not_stall_stream() {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm16kHz);
        p.audio_frame_size = AudioFrameSize(120);
        assert!(p.decode_audio(&[1]).is_empty());
        let mut decoded = 0;
        for _ in 0..100 {
            decoded += p.decode_audio(&[0u8; 120]).len();
        }
        assert_eq!(decoded, 100);
        assert_eq!(p.audio_buffer.len(), 1);
    }

    // ── parse_caps_resp_payload tests ───────────────────────────────

    #[test]
    fn test_parse_caps_resp_payload_valid() {
        // v1.0: [0x0B, version(2), codecs(1), model(1), frame_size(2)]
        let data: &[u8] = &[0x0B, 0x01, 0x00, 0x03, 0x03, 0x00, 0x80];
        let caps = parse_caps_resp_payload(data).unwrap();
        assert_eq!(caps.version, ProtocolVersion::V1_0);
        assert_eq!(caps.codecs, Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ);
        assert_eq!(caps.interaction_model, InteractionModel::HoldToTalk);
        assert_eq!(caps.audio_frame_size, AudioFrameSize(128));
    }

    #[test]
    fn test_parse_caps_resp_payload_too_short() {
        let data: &[u8] = &[0x0B, 0x01, 0x00];
        assert!(parse_caps_resp_payload(data).is_none());
    }

    // ── on_caps_resp tests ──────────────────────────────────────────

    #[test]
    fn test_on_caps_resp_selects_16khz_when_both() {
        let mut p = ProtocolV10::new();
        let caps = Capabilities {
            version: ProtocolVersion::V1_0,
            codecs: Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ,
            interaction_model: InteractionModel::HoldToTalk,
            audio_frame_size: AudioFrameSize(128),
        };
        let codec = p.on_caps_resp(&caps).unwrap();
        assert_eq!(codec, Codec::Adpcm16kHz);
        assert_eq!(p.selected_codec, Some(Codec::Adpcm16kHz));
        assert_eq!(p.interaction_model, InteractionModel::HoldToTalk);
        assert_eq!(p.audio_frame_size, AudioFrameSize(128));
    }

    #[test]
    fn test_on_caps_resp_8khz_only() {
        let mut p = ProtocolV10::new();
        let caps = Capabilities {
            version: ProtocolVersion::V1_0,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::PressToTalk,
            audio_frame_size: AudioFrameSize(20),
        };
        let codec = p.on_caps_resp(&caps).unwrap();
        assert_eq!(codec, Codec::Adpcm8kHz);
        assert_eq!(p.interaction_model, InteractionModel::PressToTalk);
    }

    #[test]
    fn test_on_caps_resp_no_common_codec() {
        let mut p = ProtocolV10::new();
        let caps = Capabilities {
            version: ProtocolVersion::V1_0,
            codecs: Codecs::empty(),
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(20),
        };
        assert!(p.on_caps_resp(&caps).is_err());
    }

    #[test]
    fn test_on_caps_resp_updates_frame_size() {
        let mut p = ProtocolV10::new();
        assert_eq!(p.audio_frame_size, AudioFrameSize::DEFAULT_V10);
        let caps = Capabilities {
            version: ProtocolVersion::V1_0,
            codecs: Codecs::ADPCM_8KHZ,
            interaction_model: InteractionModel::OnRequest,
            audio_frame_size: AudioFrameSize(160),
        };
        p.on_caps_resp(&caps).unwrap();
        assert_eq!(p.audio_frame_size, AudioFrameSize(160));
    }

    // ── Additional edge case tests ──────────────────────────────────

    #[test]
    fn test_decode_audio_wrong_size() {
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        p.audio_frame_size = AudioFrameSize(20);
        assert!(p.decode_audio(&[0u8; 10]).is_empty());
        let mut p = ProtocolV10::new();
        p.selected_codec = Some(Codec::Adpcm8kHz);
        p.audio_frame_size = AudioFrameSize(20);
        assert_eq!(p.decode_audio(&[0u8; 30]).len(), 1);
        assert!(p.decode_audio(&[]).is_empty());
    }

    #[test]
    fn test_parse_empty_ctl() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[]) {
            CtlEvent::Unknown(data) => assert!(data.is_empty()),
            other => panic!("expected Unknown for empty CTL, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_unknown_opcode() {
        let p = ProtocolV10::new();
        match p.parse_ctl(&[0xFF, 0x01, 0x02]) {
            CtlEvent::Unknown(data) => assert_eq!(data, vec![0xFF, 0x01, 0x02]),
            other => panic!("expected Unknown for unknown opcode, got {:?}", other),
        }
    }

    #[test]
    fn test_discontinuity_clears_partial_raw_and_waits_for_sync_in_headered_mode() {
        // Raw mode: clears orphan half-frame and resumes decoding cleanly.
        let mut raw = ProtocolV10::new();
        raw.selected_codec = Some(Codec::Adpcm16kHz);
        raw.audio_frame_size = AudioFrameSize(120);
        assert!(raw.decode_audio(&[0xAAu8; 60]).is_empty());
        assert!(raw.has_partial_audio());
        raw.on_audio_discontinuity();
        assert!(!raw.has_partial_audio());
        assert_eq!(raw.decode_audio(&[0u8; 120]).len(), 1);

        // Headered mode: always waits for AUDIO_SYNC after drop, even when the next
        // packet happens to be 126 bytes (could be 120 old payload + 6 new header).
        let mut hdr = ProtocolV10::new();
        hdr.selected_codec = Some(Codec::Adpcm16kHz);
        hdr.audio_frame_size = AudioFrameSize(120);
        hdr.set_frame_layout(AudioFrameLayout::Headered);
        assert!(hdr.decode_audio(&[0u8; 60]).is_empty());
        hdr.on_audio_discontinuity();
        assert!(!hdr.has_partial_audio());
        assert!(hdr.decode_audio(&[0u8; 126]).is_empty());
        assert!(hdr.decode_audio(&[0u8; 66]).is_empty());
        hdr.on_audio_sync(&AudioSyncData::Full {
            codec: Codec::Adpcm16kHz,
            seq: 5,
            predictor: 0,
            step_index: 0,
        });
        let frames = hdr.decode_audio(&[0u8; 126]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].seq, 5);
    }

    #[test]
    fn test_audio_sync_with_mismatched_sample_rate_is_ignored() {
        let mut p = ProtocolV10::new();
        let caps = Capabilities {
            version: ProtocolVersion::V1_0,
            codecs: Codecs::ADPCM_8KHZ | Codecs::ADPCM_16KHZ,
            interaction_model: InteractionModel::HoldToTalk,
            audio_frame_size: AudioFrameSize(120),
        };
        assert_eq!(p.on_caps_resp(&caps).unwrap(), Codec::Adpcm16kHz);
        p.frame_seq = 7;

        p.on_audio_sync(&AudioSyncData::Full {
            codec: Codec::Adpcm8kHz,
            seq: 99,
            predictor: 1234,
            step_index: 20,
        });

        assert_eq!(p.negotiated_codec(), Some(Codec::Adpcm16kHz));
        assert_eq!(p.frame_seq, 7);
    }
}
