#![forbid(unsafe_code)]

use std::{
    env, fs,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

#[derive(Debug)]
struct Config {
    input: PathBuf,
    output: PathBuf,
    width: u32,
    height: u32,
    fps: u32,
    codec: String,
    preset: Option<String>,
    crf: Option<u8>,
    ffmpeg: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let started_at = Instant::now();
    let config = parse_args(env::args().skip(1))?;
    let frame_size = usize::try_from(config.width)
        .ok()
        .and_then(|width| {
            usize::try_from(config.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "frame size is too large".to_owned())?;
    let parent = config.output.parent().ok_or_else(|| {
        format!(
            "output has no parent directory: {}",
            config.output.display()
        )
    })?;
    if !parent.is_dir() {
        return Err(format!(
            "output directory does not exist: {}",
            parent.display()
        ));
    }
    if config.output.exists() {
        return Err(format!(
            "refusing to overwrite output: {}",
            config.output.display()
        ));
    }

    let dimensions = format!("{}x{}", config.width, config.height);
    let (mut decoder, mut encoder) = start_ffmpeg_processes(&config, &dimensions)?;
    let mut reader = decoder
        .stdout
        .take()
        .ok_or_else(|| "decoder stdout is unavailable".to_owned())?;
    let mut writer = encoder
        .stdin
        .take()
        .ok_or_else(|| "encoder stdin is unavailable".to_owned())?;
    let mut frame = vec![0_u8; frame_size];
    let mut frame_count = 0_u64;
    let stream_result = stream_frames(&mut reader, &mut writer, &mut frame, &mut frame_count);
    drop(writer);
    stream_result?;

    finish_processes(&mut decoder, &mut encoder, &config.output, frame_count)?;
    println!(
        "input={} output={} frames={} size={} fps={} codec={} format=matroska elapsed_ms={}",
        config.input.display(),
        config.output.display(),
        frame_count,
        dimensions,
        config.fps,
        config.codec,
        started_at.elapsed().as_millis()
    );
    Ok(())
}

fn stream_frames(
    reader: &mut impl Read,
    writer: &mut impl Write,
    frame: &mut [u8],
    frame_count: &mut u64,
) -> Result<(), String> {
    while read_frame(reader, frame)? {
        writer
            .write_all(frame)
            .map_err(|error| format!("write BGR frame to encoder: {error}"))?;
        *frame_count = frame_count.saturating_add(1);
    }
    Ok(())
}

fn start_ffmpeg_processes(
    config: &Config,
    dimensions: &str,
) -> Result<(std::process::Child, std::process::Child), String> {
    let decoder = Command::new(&config.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            config
                .input
                .to_str()
                .ok_or_else(|| "input path is not UTF-8".to_owned())?,
            "-vf",
            &format!("scale={dimensions}:flags=area"),
            "-pix_fmt",
            "bgr24",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start decoder ffmpeg: {error}"))?;
    let mut encoder_args = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-y".to_owned(),
        "-f".to_owned(),
        "rawvideo".to_owned(),
        "-pixel_format".to_owned(),
        "bgr24".to_owned(),
        "-video_size".to_owned(),
        dimensions.to_owned(),
        "-framerate".to_owned(),
        config.fps.to_string(),
        "-i".to_owned(),
        "pipe:0".to_owned(),
        "-an".to_owned(),
        "-c:v".to_owned(),
        config.codec.clone(),
    ];
    if let Some(preset) = &config.preset {
        encoder_args.extend(["-preset".to_owned(), preset.clone()]);
    }
    if let Some(crf) = config.crf {
        encoder_args.extend(["-crf".to_owned(), crf.to_string()]);
    }
    encoder_args.extend([
        "-f".to_owned(),
        "matroska".to_owned(),
        config
            .output
            .to_str()
            .ok_or_else(|| "output path is not UTF-8".to_owned())?
            .to_owned(),
    ]);
    let encoder = Command::new(&config.ffmpeg)
        .args(encoder_args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start encoder ffmpeg: {error}"))?;

    Ok((decoder, encoder))
}

fn finish_processes(
    decoder: &mut std::process::Child,
    encoder: &mut std::process::Child,
    output: &PathBuf,
    frame_count: u64,
) -> Result<(), String> {
    let decoder_status = decoder
        .wait()
        .map_err(|error| format!("wait decoder ffmpeg: {error}"))?;
    let encoder_status = encoder
        .wait()
        .map_err(|error| format!("wait encoder ffmpeg: {error}"))?;
    if !decoder_status.success() {
        let _ = fs::remove_file(output);
        return Err(format!("decoder ffmpeg exited with {decoder_status}"));
    }
    if !encoder_status.success() {
        let _ = fs::remove_file(output);
        return Err(format!("encoder ffmpeg exited with {encoder_status}"));
    }
    if frame_count == 0 {
        let _ = fs::remove_file(output);
        return Err("input produced no video frames".to_owned());
    }
    Ok(())
}

fn read_frame(reader: &mut impl Read, frame: &mut [u8]) -> Result<bool, String> {
    let mut offset = 0;
    while offset < frame.len() {
        match reader.read(&mut frame[offset..]) {
            Ok(0) if offset == 0 => return Ok(false),
            Ok(0) => return Err("decoder emitted a truncated raw video frame".to_owned()),
            Ok(read) => offset += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("read BGR frame from decoder: {error}")),
        }
    }
    Ok(true)
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<Config, String> {
    let mut input = None;
    let mut output = None;
    let mut width = 256;
    let mut height = 256;
    let mut fps = 25;
    let mut codec = "ffv1".to_owned();
    let mut preset = None;
    let mut crf = None;
    let mut ffmpeg = "ffmpeg".to_owned();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--input" => input = Some(PathBuf::from(argument_value(&argument, &mut arguments)?)),
            "--output" => output = Some(PathBuf::from(argument_value(&argument, &mut arguments)?)),
            "--width" => width = positive_u32(&argument, &mut arguments)?,
            "--height" => height = positive_u32(&argument, &mut arguments)?,
            "--fps" => fps = positive_u32(&argument, &mut arguments)?,
            "--codec" => codec = argument_value(&argument, &mut arguments)?,
            "--preset" => preset = Some(argument_value(&argument, &mut arguments)?),
            "--crf" => crf = Some(crf_value(&argument, &mut arguments)?),
            "--ffmpeg" => ffmpeg = argument_value(&argument, &mut arguments)?,
            "--help" => return Err(usage().to_owned()),
            _ => return Err(format!("unknown argument {argument}\n{}", usage())),
        }
    }
    Ok(Config {
        input: input.ok_or_else(|| format!("--input is required\n{}", usage()))?,
        output: output.ok_or_else(|| format!("--output is required\n{}", usage()))?,
        width,
        height,
        fps,
        codec,
        preset,
        crf,
        ffmpeg,
    })
}

fn argument_value(
    name: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{name} requires a value"))
}

fn positive_u32(name: &str, arguments: &mut impl Iterator<Item = String>) -> Result<u32, String> {
    argument_value(name, arguments)?
        .parse()
        .ok()
        .filter(|value: &u32| *value > 0)
        .ok_or_else(|| format!("{name} must be a positive integer"))
}

fn crf_value(name: &str, arguments: &mut impl Iterator<Item = String>) -> Result<u8, String> {
    argument_value(name, arguments)?
        .parse()
        .ok()
        .filter(|value: &u8| *value <= 51)
        .ok_or_else(|| format!("{name} must be an integer from 0 to 51"))
}

const fn usage() -> &'static str {
    "usage: pcrt-video-reencode --input VIDEO --output OUTPUT.mkv [--width 256] [--height 256] [--fps 25] [--codec ffv1] [--preset PRESET] [--crf 0..51] [--ffmpeg ffmpeg]"
}
