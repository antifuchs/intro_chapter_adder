//! Detect silence / blackness on an input file
use crate::util::to_duration;
use anyhow::{bail, Context, Result};
use ffmpeg::{codec, filter, format, frame, media};
use indicatif::{HumanDuration, ProgressBar};
use std::{fmt::Debug, time::Duration};

#[derive(Debug, PartialEq)]
pub(crate) enum Candidate {
    Darkness { offset: Duration, length: Duration },
    Silence { offset: Duration, length: Duration },
}

#[derive(Debug, PartialEq)]
enum DetectState {
    None,
    Start(Duration),
}

pub(crate) struct Detector {
    audio_stream: usize,
    video_stream: usize,
    audio_filter: filter::Graph,
    audio_decoder: codec::decoder::Audio,
    video_filter: filter::Graph,
    video_decoder: codec::decoder::Video,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum StreamState {
    Ongoing,
    EndOfAudio,
    EndOfVideo,
    End,
}

impl Default for StreamState {
    fn default() -> Self {
        StreamState::Ongoing
    }
}

impl StreamState {
    fn end_of_audio(self) -> Self {
        use StreamState::*;
        match self {
            EndOfVideo => End,
            _ => EndOfAudio,
        }
    }

    fn end_of_video(self) -> Self {
        use StreamState::*;
        match self {
            EndOfAudio => End,
            _ => EndOfVideo,
        }
    }

    fn reading_audio(&self) -> bool {
        use StreamState::*;
        match self {
            End | EndOfAudio => false,
            _ => true,
        }
    }

    fn reading_video(&self) -> bool {
        use StreamState::*;
        match self {
            End | EndOfVideo => false,
            _ => true,
        }
    }

    fn at_end(&self) -> bool {
        *self == StreamState::End
    }
}

impl Detector {
    pub(crate) fn detect(
        &mut self,
        ictx: &mut format::context::Input,
        until: Duration,
        bar: &ProgressBar,
    ) -> Result<Vec<Candidate>> {
        let mut candidates = vec![];
        let in_time_base = self.audio_decoder.time_base();
        let mut audio = frame::Audio::empty();
        let mut video = frame::Video::empty();
        let mut state = StreamState::default();

        let mut audio_state = DetectState::None;
        let mut video_state = DetectState::None;

        for (stream, mut packet) in ictx.packets() {
            if state.reading_audio() && stream.index() == self.audio_stream {
                packet.rescale_ts(stream.time_base(), in_time_base);
                if let Ok(true) = self.audio_decoder.decode(&packet, &mut audio) {
                    let timestamp = audio.timestamp();
                    audio.set_pts(timestamp);
                    if let Some(timestamp) = timestamp {
                        let at_ts = to_duration(timestamp, in_time_base);
                        if at_ts >= until {
                            state = state.end_of_audio();
                        }
                    }
                    self.audio_filter
                        .get("in")
                        .unwrap()
                        .source()
                        .add(&audio)
                        .unwrap();
                    while let Ok(..) = self
                        .audio_filter
                        .get("out")
                        .unwrap()
                        .sink()
                        .frame(&mut audio)
                    {
                        // read audio frame-by-frame:
                        let meta = audio.metadata();
                        match (
                            &audio_state,
                            meta.get("lavfi.silence_start"),
                            meta.get("lavfi.silence_duration"),
                            audio.timestamp(),
                        ) {
                            (_, None, None, _) => {}
                            (DetectState::None, Some(_), None, Some(ts)) => {
                                audio_state = DetectState::Start(to_duration(ts, in_time_base));
                            }
                            (DetectState::Start(offset), None, Some(duration), _) => {
                                let length_f64 = duration.parse()?;
                                let length = Duration::from_secs_f64(length_f64);
                                bar.set_message(&format!("silence at {}", HumanDuration(*offset)));
                                candidates.push(Candidate::Silence {
                                    offset: *offset,
                                    length,
                                });
                                audio_state = DetectState::None;
                            }
                            combo => {
                                bail!("Unclear combination of audio things: {:?}", combo);
                            }
                        }
                    }
                }
            }
            if state.reading_video() && stream.index() == self.video_stream {
                packet.rescale_ts(stream.time_base(), in_time_base);
                if let Ok(true) = self.video_decoder.decode(&packet, &mut video) {
                    let timestamp = video.timestamp();
                    video.set_pts(timestamp);
                    if let Some(ts) = timestamp {
                        let at_ts = to_duration(ts, in_time_base);
                        if at_ts >= until {
                            state = state.end_of_video();
                        }
                        bar.set_position(at_ts.as_millis() as u64);
                    }
                    self.video_filter
                        .get("in")
                        .unwrap()
                        .source()
                        .add(&video)
                        .unwrap();
                    while let Ok(..) = self
                        .video_filter
                        .get("out")
                        .unwrap()
                        .sink()
                        .frame(&mut video)
                    {
                        // read video frame-by-frame:
                        let meta = video.metadata();
                        match (
                            &video_state,
                            meta.get("lavfi.black_start"),
                            meta.get("lavfi.black_end"),
                            video.timestamp(),
                        ) {
                            (_, None, None, _) => {}
                            (DetectState::None, Some(_), None, Some(ts)) => {
                                video_state = DetectState::Start(to_duration(ts, in_time_base));
                            }
                            (DetectState::Start(offset), None, Some(duration), _) => {
                                let length_f64 = duration.parse()?;
                                let length = Duration::from_secs_f64(length_f64) - *offset;
                                bar.set_message(&format!("darkness at {}", HumanDuration(*offset)));
                                candidates.push(Candidate::Darkness {
                                    offset: *offset,
                                    length,
                                });
                                video_state = DetectState::None;
                            }
                            combo => {
                                bail!("Unclear combination of video things: {:?}", combo);
                            }
                        }
                    }
                }
            }
            if state.at_end() {
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
            self.audio_stream, self.video_stream
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
        audio_stream: audio.index(),
        video_stream: video.index(),
        audio_filter,
        audio_decoder,
        video_filter,
        video_decoder,
    })
}
