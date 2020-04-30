extern crate ffmpeg4 as ffmpeg;
use anyhow::{self, bail, Context};
use detect::Candidate;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use mktemp::Temp;
use rayon::prelude::*;
use serde_derive::*;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{thread, time::Duration};

mod detect;
mod util;

#[derive(Debug, PartialEq, Deserialize)]
struct TitleInfo {
    location: PathBuf,
    theme_start: f64,
    theme_end: f64,
}

#[derive(Debug, structopt::StructOpt)]
#[structopt(
    name = "intro_chapter_adder",
    about = "A thing dealing with annoying intros on TV shows."
)]
enum Options {
    /// Add chapter markers from a CSV file.
    AddChapterMarkers,

    /// Detect silences in the first few minutes and add markers for them
    DetectSilence {
        /// The MKV files to treat
        #[structopt(parse(from_os_str))]
        paths: Vec<PathBuf>,

        /// Scan this long into the beginning of the file
        #[structopt(
            long = "--until",
            default_value = "10m",
            parse(try_from_str = humantime::parse_duration)
        )]
        until: Duration,

        /// Only consider pauses this long or longer as real "breaks"
        #[structopt(
            long = "--threshold",
            default_value = "200ms",
            parse(try_from_str = humantime::parse_duration)
        )]
        threshold: Duration,

        /// Actually write chapter markers. NOTE: This overwrites any existing chapters.
        #[structopt(long = "--do-it", short = "-f")]
        do_it: bool,
    },
}

#[paw::main]
fn main(args: Options) -> anyhow::Result<()> {
    let base = PathBuf::from("/Volumes/Media");

    ffmpeg::init()?;
    unsafe {
        ffmpeg::ffi::av_log_set_level(ffmpeg::ffi::AV_LOG_WARNING);
    }

    match args {
        Options::AddChapterMarkers => {
            let mut rdr = csv::Reader::from_reader(io::stdin());
            for result in rdr.deserialize() {
                let record: TitleInfo = result?;
                println!("{:?}", record);
                adjust_tags_on(&base, record)?;
            }
            Ok(())
        }
        Options::DetectSilence {
            paths,
            until,
            threshold,
            do_it,
        } => {
            let multibar = MultiProgress::new();
            let sty = ProgressStyle::default_bar().template(
                "[{prefix}:{elapsed_precise}] {bar:30.cyan/blue} {pos:>7}ms/{len:7}ms [ETA:{eta}]",
            );
            let progress_paths: Vec<(ProgressBar, &PathBuf)> = paths
                .iter()
                .map(|path| {
                    let bar = multibar.add(ProgressBar::new(until.as_millis() as u64));
                    bar.set_style(sty.clone());
                    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                        let name: String = name.chars().take(50).collect();
                        bar.set_prefix(&name);
                    }
                    bar
                })
                .zip(paths.iter())
                .collect();
            thread::spawn(move || multibar.join_and_clear());
            progress_paths
                .into_par_iter()
                .map(|(bar, path)| {
                    let mut ictx = ffmpeg::format::input(&path)
                        .context(format!("opening input file {:?}", &path))?;
                    let detector = detect::detector(&mut ictx)?;
                    let candidates: Vec<Candidate> = detector
                        .markers(&mut ictx, until, &bar)?
                        .filter(|cand| {
                            cand.offset > Duration::from_secs(1) && cand.length > threshold
                        })
                        .collect();
                    if do_it {
                        set_chapters(&path, candidates.iter().enumerate().map(|c| c.into()))
                    } else {
                        let chapters: Vec<Chapter> =
                            candidates.iter().enumerate().map(|c| c.into()).collect();
                        bar.println(format!("would set chapters on {:?}:", &path));
                        for c in chapters {
                            bar.println(format!("{}", c));
                        }
                        Ok(())
                    }
                })
                .collect()
        }
    }
}

#[derive(PartialEq, Debug)]
struct Chapter {
    id: usize,
    start: Duration,
    name: String,
}

impl Chapter {
    fn from_ffmpeg(id: usize, chapter: ffmpeg::format::chapter::Chapter) -> Self {
        Chapter {
            id,
            start: util::to_duration(chapter.start(), chapter.time_base()),
            name: chapter
                .metadata()
                .get("title")
                .unwrap_or("untitled")
                .to_string(),
        }
    }

    fn new(id: usize, start: Duration, name: String) -> Self {
        Chapter { id, start, name }
    }
}

impl From<(usize, &Candidate)> for Chapter {
    fn from(f: (usize, &Candidate)) -> Self {
        Chapter {
            id: f.0,
            start: f.1.offset,
            name: format!("Silence {}", f.0 + 1),
        }
    }
}

impl fmt::Display for Chapter {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let secs = self.start.as_secs();
        writeln!(
            f,
            "CHAPTER{:0>2}={:0>2}:{:0>2}:{:0>2}.{}",
            self.id,
            secs / 60 / 60,
            secs / 60,
            secs % 60,
            self.start.subsec_millis()
        )?;
        write!(f, "CHAPTER{:0>2}NAME={}", self.id, self.name)?;
        Ok(())
    }
}

fn existing_chapters(path: &Path) -> anyhow::Result<Vec<Chapter>> {
    let ictx = ffmpeg::format::input(&path)?;
    Ok(ictx
        .chapters()
        .enumerate()
        .map(|(i, chapter)| Chapter::from_ffmpeg(i, chapter))
        .collect())
}

fn adjust_tags_on(base: &Path, title_info: TitleInfo) -> anyhow::Result<()> {
    let input = base.join(title_info.location.strip_prefix("/media")?);
    let mut chapters = existing_chapters(&input)?;
    let (theme_start, theme_end) = (
        Duration::from_secs_f64(title_info.theme_start),
        Duration::from_secs_f64(title_info.theme_end),
    );

    chapters.push(Chapter::new(
        chapters.len(),
        theme_start,
        "Start of intro".to_string(),
    ));
    chapters.push(Chapter::new(
        chapters.len(),
        theme_end,
        "End of intro".to_string(),
    ));
    set_chapters(&input, chapters)
}

fn set_chapters(
    mkv_file: &Path,
    chapters: impl IntoIterator<Item = Chapter>,
) -> anyhow::Result<()> {
    let tmpfile = Temp::new_file()?;
    let f = File::create(tmpfile.as_path())?;
    let mut w = BufWriter::new(f);
    for ch in chapters.into_iter() {
        writeln!(&mut w, "{}", ch)?;
    }
    w.into_inner()?.sync_all()?;

    let output = Command::new("mkvpropedit")
        .arg(&mkv_file)
        .arg("--chapters")
        .arg(tmpfile.as_path())
        .output()?;
    if !output.status.success() {
        bail!(
            "unsuccessful for {:?} - mkv chapter contents:\n{:?}\n\nmkvpropedit stdout:\n{:?}\nstderr:\n{:?}",
            mkv_file,
            fs::read_to_string(tmpfile.as_path()).unwrap_or("unreadable".to_string()),
            output.stdout,
            output.stderr
        );
    }

    Ok(())
}
