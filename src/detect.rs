//! Detect silence / blackness on an input file
use crate::util::to_duration;
use anyhow::{Context, Result};
use ffmpeg::{codec, filter, format, frame, media, Packet, Rational, Stream};
use indicatif::{HumanDuration, ProgressBar};
use std::{cmp::max, fmt::Debug, time::Duration};

/// A spot in the video where there's both a blank (black) screen and
/// a silence.
#[derive(PartialEq)]
pub(crate) struct Candidate {
    pub(crate) offset: Duration,
    length: Duration,
}

impl Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{ {} for {} }}",
            humantime::format_duration(self.offset),
            humantime::format_duration(self.length)
        )
    }
}

impl Candidate {
    pub(crate) fn new(offset: Duration, length: Duration) -> Self {
        Self { offset, length }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum DetectState {
    None,
    Video(Duration),
    Audio(Duration),
    VideoAndAudio { video: Duration, audio: Duration },
}

pub(crate) struct Detector {
    audio: SilenceDetector,
    video: BlankDetector,
}

impl Detector {
    pub(crate) fn detect(
        &mut self,
        ictx: &mut format::context::Input,
        until: Duration,
        threshold: Duration,
        bar: &ProgressBar,
    ) -> Result<Vec<Candidate>> {
        let mut candidates = vec![];
        let mut blank_state = DetectState::None;

        for (stream, mut packet) in ictx.packets() {
            self.audio
                .detected_pauses_from_packet(&stream, &mut packet, until, bar, |pause| {
                    blank_state = match (pause, blank_state) {
                        (PauseMatch::None, s) => s,
                        (PauseMatch::Start(d), DetectState::None) => DetectState::Audio(d),
                        (PauseMatch::Start(audio), DetectState::Video(video)) => {
                            DetectState::VideoAndAudio { video, audio }
                        }
                        (PauseMatch::End(end), DetectState::VideoAndAudio { video, audio }) => {
                            let offset = max(video, audio);
                            let length = end - audio;
                            bar.set_message(&format!(
                                "quiet blackness at {}",
                                HumanDuration(offset)
                            ));
                            if length > threshold {
                                candidates.push(Candidate::new(offset, length));
                            }
                            DetectState::Video(video)
                        }
                        (PauseMatch::End(_), DetectState::Audio(_)) => DetectState::None,
                        combo => {
                            unreachable!("Unclear combination of audio circumstances: {:?}", combo);
                        }
                    };
                })?;

            self.video
                .detected_pauses_from_packet(&stream, &mut packet, until, bar, |pause| {
                    blank_state = match (pause, blank_state) {
                        (PauseMatch::None, s) => s,
                        (PauseMatch::Start(d), DetectState::None) => DetectState::Video(d),
                        (PauseMatch::Start(video), DetectState::Audio(audio)) => {
                            DetectState::VideoAndAudio { audio, video }
                        }
                        (PauseMatch::End(end), DetectState::VideoAndAudio { audio, video }) => {
                            let offset = max(audio, video);
                            let length = end - video;
                            bar.set_message(&format!(
                                "quiet blackness at {}",
                                HumanDuration(offset)
                            ));
                            if length > threshold {
                                candidates.push(Candidate::new(offset, length));
                            }
                            DetectState::Audio(audio)
                        }
                        (PauseMatch::End(_), DetectState::Video(_)) => DetectState::None,
                        combo => {
                            unreachable!("Unclear combination of video circumstances: {:?}", combo);
                        }
                    }
                })?;

            if self.video.at_end && self.audio.at_end {
                break;
            }
        }
        Ok(candidates)
    }
}

impl Debug for Detector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Detector {{ audio_stream: {:?}, video_stream: {:?} }}",
            self.audio.audio_stream, self.video.video_stream
        )
    }
}

pub(crate) fn detector(ictx: &mut format::context::Input) -> Result<Detector> {
    let audio = ictx
        .streams()
        .best(media::Type::Audio)
        .context("finding 'best' audio stream")?;

    // AV decoding:
    let mut audio_decoder = audio
        .codec()
        .decoder()
        .audio()
        .context("getting an audio decoder")?;
    audio_decoder.set_parameters(audio.parameters())?;

    let video = ictx
        .streams()
        .best(media::Type::Video)
        .context("finding 'video' audio stream")?;
    let mut video_decoder = video
        .codec()
        .decoder()
        .video()
        .context("getting a video decoder")?;
    video_decoder.set_parameters(video.parameters())?;

    // audio filter chain:
    let mut audio_filter = filter::Graph::new();
    let audio_args = format!(
        "time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        audio_decoder.time_base(),
        audio_decoder.rate(),
        audio_decoder.format().name(),
        audio_decoder.channel_layout().bits(),
    );
    audio_filter.add(&filter::find("abuffer").unwrap(), "in", &audio_args)?;
    audio_filter.add(&filter::find("abuffersink").unwrap(), "out", "")?;
    audio_filter
        .output("in", 0)?
        .input("out", 0)?
        .parse("silencedetect=n=-50dB:d=0.3")?;
    audio_filter.validate().context("validating audio filter")?;

    // video filter chain:
    let mut video_filter = filter::Graph::new();
    let video_args = format!(
        "time_base={}:frame_rate={}:width={}:height={}:pix_fmt={}",
        video_decoder.time_base(),
        video_decoder.frame_rate().expect("Frame rate not known"),
        video_decoder.width(),
        video_decoder.height(),
        video_decoder
            .format()
            .descriptor()
            .expect("Pixel format descriptor not known")
            .name(),
    );
    video_filter.add(&filter::find("buffer").unwrap(), "in", &video_args)?;
    video_filter.add(&filter::find("buffersink").unwrap(), "out", "")?;
    video_filter
        .output("in", 0)?
        .input("out", 0)?
        .parse("blackdetect=d=0.5:pix_th=0.1")?;
    video_filter.validate().context("validating video filter")?;

    Ok(Detector {
        audio: SilenceDetector::new(audio.index(), audio_filter, audio_decoder),
        video: BlankDetector::new(video.index(), video_filter, video_decoder),
    })
}

#[derive(PartialEq, Debug, Clone, Copy)]
enum PauseMatch {
    None,
    Start(Duration),
    End(Duration),
}

trait PauseDetector {
    fn detected_pauses_from_packet(
        &mut self,
        stream: &Stream,
        packet: &mut Packet,
        until: Duration,
        bar: &ProgressBar,
        mut callback: impl FnMut(PauseMatch),
    ) -> Result<Option<bool>> {
        if !self.is_applicable_stream(&stream) {
            return Ok(Some(false));
        }
        if self.is_at_end() {
            return Ok(None);
        }
        let in_time_base = self.time_base();
        packet.rescale_ts(stream.time_base(), in_time_base);

        let mut frame = Self::empty_frame();
        if let (Ok(true), timestamp) = self.decode(&packet, &mut frame) {
            if let Some(timestamp) = timestamp {
                let at_ts = to_duration(timestamp, in_time_base);
                if at_ts >= until {
                    self.set_at_end();
                }
                Self::update_progress(bar, at_ts.as_millis() as u64);
            }
            self.filter_frame_in(&frame).unwrap();
            while let Ok(..) = self.filter_frame_output(&mut frame) {
                callback(self.frame_matches(&frame));
            }
        }
        Ok(Some(true))
    }

    fn update_progress(_bar: &ProgressBar, _position: u64) {}

    type FrameType;

    fn is_applicable_stream(&self, stream: &Stream) -> bool;

    fn set_at_end(&mut self);

    fn is_at_end(&self) -> bool;

    fn time_base(&self) -> Rational;

    fn empty_frame() -> Self::FrameType;

    fn decode(
        &mut self,
        packet: &Packet,
        frame: &mut Self::FrameType,
    ) -> (Result<bool, ffmpeg::Error>, Option<i64>);

    fn filter_frame_in(&mut self, frame: &Self::FrameType) -> Result<(), ffmpeg::Error>;

    fn filter_frame_output(&mut self, frame: &mut Self::FrameType) -> Result<(), ffmpeg::Error>;

    fn frame_matches(&mut self, frame: &Self::FrameType) -> PauseMatch;
}

struct SilenceDetector {
    audio_stream: usize,
    time_base: Rational,
    audio_filter: filter::Graph,
    audio_decoder: codec::decoder::Audio,
    at_end: bool,
    inside_pause: bool,
}

impl SilenceDetector {
    fn new(
        audio_stream: usize,
        mut audio_filter: filter::Graph,
        audio_decoder: codec::decoder::Audio,
    ) -> Self {
        audio_filter.validate().expect("audio filter can't work!");
        Self {
            audio_stream,
            time_base: audio_decoder.time_base(),
            audio_filter,
            audio_decoder,
            at_end: false,
            inside_pause: false,
        }
    }
}

impl PauseDetector for SilenceDetector {
    fn is_applicable_stream(&self, stream: &Stream) -> bool {
        self.audio_stream == stream.index()
    }

    fn set_at_end(&mut self) {
        self.at_end = true;
    }

    fn is_at_end(&self) -> bool {
        self.at_end
    }

    fn time_base(&self) -> Rational {
        self.time_base
    }
    type FrameType = frame::Audio;

    fn empty_frame() -> Self::FrameType {
        frame::Audio::empty()
    }

    fn decode(
        &mut self,
        packet: &Packet,
        mut frame: &mut Self::FrameType,
    ) -> (Result<bool, ffmpeg::Error>, Option<i64>) {
        let result = self.audio_decoder.decode(packet, &mut frame);
        if let Ok(true) = result {
            return (Ok(true), frame.timestamp());
        }
        (result, None)
    }

    fn filter_frame_in(&mut self, frame: &Self::FrameType) -> Result<(), ffmpeg::Error> {
        self.audio_filter.get("in").unwrap().source().add(&frame)
    }

    fn filter_frame_output(
        &mut self,
        mut frame: &mut Self::FrameType,
    ) -> Result<(), ffmpeg::Error> {
        self.audio_filter
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut frame)
    }

    fn frame_matches(&mut self, audio: &Self::FrameType) -> PauseMatch {
        let meta = audio.metadata();
        match (
            self.inside_pause,
            meta.get("lavfi.silence_start"),
            meta.get("lavfi.silence_duration"),
            audio.timestamp(),
        ) {
            (_, None, None, _) => PauseMatch::None,
            (_, Some(_), Some(_), _) => PauseMatch::None, // skip super short silences
            (true, None, Some(_), Some(ts)) => {
                self.inside_pause = false;
                PauseMatch::End(to_duration(ts, self.time_base))
            }
            (false, Some(_), None, Some(ts)) => {
                self.inside_pause = true;
                PauseMatch::Start(to_duration(ts, self.time_base))
            }
            combo => {
                panic!("Unclear combination of audio things: {:?}", combo);
            }
        }
    }
}

struct BlankDetector {
    video_stream: usize,
    time_base: Rational,
    video_filter: filter::Graph,
    video_decoder: codec::decoder::Video,
    at_end: bool,
    inside_pause: bool,
}

impl BlankDetector {
    fn new(
        video_stream: usize,
        mut video_filter: filter::Graph,
        video_decoder: codec::decoder::Video,
    ) -> Self {
        video_filter.validate().expect("video filter can't work!");
        Self {
            video_stream,
            time_base: video_decoder.time_base(),
            video_filter,
            video_decoder,
            at_end: false,
            inside_pause: false,
        }
    }
}

impl PauseDetector for BlankDetector {
    type FrameType = ffmpeg::frame::Video;

    fn update_progress(bar: &ProgressBar, position: u64) {
        bar.set_position(position);
    }

    fn is_applicable_stream(&self, stream: &Stream) -> bool {
        stream.index() == self.video_stream
    }

    fn set_at_end(&mut self) {
        self.at_end = true;
    }

    fn is_at_end(&self) -> bool {
        self.at_end
    }

    fn time_base(&self) -> Rational {
        self.time_base
    }

    fn empty_frame() -> Self::FrameType {
        ffmpeg::frame::Video::empty()
    }

    fn decode(
        &mut self,
        packet: &Packet,
        mut frame: &mut Self::FrameType,
    ) -> (Result<bool, ffmpeg::Error>, Option<i64>) {
        let result = self.video_decoder.decode(packet, &mut frame);
        if let Ok(true) = result {
            return (Ok(true), frame.timestamp());
        }
        (result, None)
    }

    fn filter_frame_in(&mut self, frame: &Self::FrameType) -> Result<(), ffmpeg::Error> {
        self.video_filter.get("in").unwrap().source().add(&frame)
    }

    fn filter_frame_output(
        &mut self,
        mut frame: &mut Self::FrameType,
    ) -> Result<(), ffmpeg::Error> {
        self.video_filter
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut frame)
    }

    fn frame_matches(&mut self, video: &Self::FrameType) -> PauseMatch {
        let meta = video.metadata();
        match (
            self.inside_pause,
            meta.get("lavfi.black_start"),
            meta.get("lavfi.black_end"),
            video.timestamp(),
        ) {
            (_, None, None, _) => PauseMatch::None,
            (_, Some(_), Some(_), _) => PauseMatch::None, // skip super short silences
            (true, None, Some(_), Some(ts)) => {
                self.inside_pause = false;
                PauseMatch::End(to_duration(ts, self.time_base))
            }
            (false, Some(_), None, Some(ts)) => {
                self.inside_pause = true;
                PauseMatch::Start(to_duration(ts, self.time_base))
            }
            combo => {
                panic!("Unclear combination of audio things: {:?}", combo);
            }
        }
    }
}
