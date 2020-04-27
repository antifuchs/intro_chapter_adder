extern crate ffmpeg4 as ffmpeg;
use anyhow::{self, bail, Context};
use mktemp::Temp;
use serde_derive::*;
use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
    AddChapterMarkers,
    DetectSilence {
        #[structopt(parse(from_os_str))]
        path: PathBuf,
    },
}

#[paw::main]
fn main(args: Options) -> anyhow::Result<()> {
    let base = PathBuf::from("/Volumes/Media");

    ffmpeg::init()?;
    match args {
        Options::AddChapterMarkers => {
            let mut rdr = csv::Reader::from_reader(io::stdin());
            for result in rdr.deserialize() {
                let record: TitleInfo = result?;
                println!("{:?}", record);
                adjust_tags_on(&base, record)?;
            }
        }
        Options::DetectSilence { path } => {
            let mut ictx =
                ffmpeg::format::input(&path).context(format!("opening input file {:?}", &path))?;
            let mut detector = detect::detector(&mut ictx)?;
            let candidates = detector.detect(&mut ictx, Duration::from_secs(600))?;
            println!("Silences: {:?}", candidates);
        }
    }
    Ok(())
}

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

    let tmpfile = Temp::new_file()?;
    let f = File::create(tmpfile.as_path())?;
    let mut w = BufWriter::new(f);
    for ch in chapters {
        writeln!(&mut w, "{}", ch)?;
        println!("{}", ch);
    }
    w.into_inner()?.sync_all()?;

    let success = Command::new("mkvpropedit")
        .arg(&input)
        .arg("--chapters")
        .arg(tmpfile.as_path())
        .status()?
        .success();
    if !success {
        bail!("unsuccessful for {:?}", input);
    }

    Ok(())
}
