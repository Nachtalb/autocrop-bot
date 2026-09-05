//! autocrop-bot — Telegram bot that crops screenshots down to their content.
//!
//! Photos and image files go through the `autocrop` detector directly. Videos,
//! GIFs and video files get a probe frame extracted with ffmpeg, the detector
//! runs on that frame, and ffmpeg then crops the whole clip to the rectangle.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use autocrop::{CropResult, Encoding, Params, RgbImage, find_crop};
use teloxide::dispatching::UpdateFilterExt;
use teloxide::net::Download;
use teloxide::payloads::{
    SendAnimationSetters, SendDocumentSetters, SendMessageSetters, SendPhotoSetters,
    SendVideoSetters,
};
use teloxide::prelude::*;
use teloxide::types::{FileId, InputFile, MediaKind, MessageKind, ReplyParameters};
use teloxide::utils::command::BotCommands;

/// Public Bot API download cap.
const MAX_DOWNLOAD: u32 = 20 * 1024 * 1024;

/// Mean luminance (0-255) under which a probe frame counts as black.
const BLACK_LUMA: f32 = 20.0;

#[derive(BotCommands, Clone, Debug, PartialEq, Eq)]
#[command(
    rename_rule = "lowercase",
    description = "These commands are supported:"
)]
pub enum Command {
    #[command(description = "show this help text.")]
    Help,
    #[command(description = "what this bot does.")]
    Start,
}

const ABOUT: &str = "Send me a screenshot as a photo, video, GIF or file. I send back just the \
picture: no status bars, app chrome, black bars or meme text. Up to 20 MB.";

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,teloxide=info")),
        )
        .init();

    let token = std::env::var("TELOXIDE_TOKEN").context("TELOXIDE_TOKEN must be set")?;
    let bot = Bot::new(token);

    if let Err(err) = publish_bot_metadata(&bot).await {
        tracing::warn!(?err, "failed to publish bot metadata on startup");
    }

    let handler = Update::filter_message().branch(
        dptree::entry()
            .branch(
                dptree::entry()
                    .filter_command::<Command>()
                    .endpoint(handle_command),
            )
            .branch(dptree::endpoint(handle_media)),
    );

    tracing::info!("starting long-polling dispatcher");
    Dispatcher::builder(bot, handler)
        .default_handler(|_| async {})
        .error_handler(LoggingErrorHandler::with_custom_text(
            "an error in the update handler",
        ))
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
    Ok(())
}

async fn publish_bot_metadata(bot: &Bot) -> Result<()> {
    use teloxide::payloads::{
        SetMyDescriptionSetters, SetMyNameSetters, SetMyShortDescriptionSetters,
    };
    bot.set_my_commands(Command::bot_commands()).await?;
    bot.set_my_short_description()
        .short_description("Cuts screenshots down to the picture.")
        .await?;
    bot.set_my_description().description(ABOUT).await?;
    // Telegram rate-limits name changes hard; a failure here must not block startup.
    if let Err(err) = bot.set_my_name().name("Autocrop").await {
        tracing::warn!(?err, "set_my_name failed");
    }
    Ok(())
}

async fn handle_command(bot: Bot, msg: Message, cmd: Command) -> Result<()> {
    let text = match cmd {
        Command::Help => Command::descriptions().to_string(),
        Command::Start => ABOUT.to_string(),
    };
    bot.send_message(msg.chat.id, text)
        .reply_parameters(ReplyParameters::new(msg.id))
        .await?;
    Ok(())
}

/// How the input arrived; decides how the result is sent back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Photo,
    Video,
    Animation,
    Document,
}

struct Incoming {
    file_id: FileId,
    size: Option<u32>,
    name: String,
    source: Source,
}

fn extract(msg: &Message) -> Option<Incoming> {
    let MessageKind::Common(common) = &msg.kind else {
        return None;
    };
    let (file, name, source) = match &common.media_kind {
        MediaKind::Photo(p) => {
            let best = p.photo.iter().max_by_key(|s| s.file.size)?;
            (&best.file, String::new(), Source::Photo)
        }
        MediaKind::Video(v) => (
            &v.video.file,
            v.video.file_name.clone().unwrap_or_default(),
            Source::Video,
        ),
        MediaKind::Animation(a) => (
            &a.animation.file,
            a.animation.file_name.clone().unwrap_or_default(),
            Source::Animation,
        ),
        MediaKind::Document(d) => (
            &d.document.file,
            d.document.file_name.clone().unwrap_or_default(),
            Source::Document,
        ),
        _ => return None,
    };
    Some(Incoming {
        file_id: file.id.clone(),
        size: Some(file.size),
        name,
        source,
    })
}

async fn handle_media(bot: Bot, msg: Message) -> Result<()> {
    let Some(incoming) = extract(&msg) else {
        if msg.chat.is_private() {
            reply(
                &bot,
                &msg,
                "Send me a screenshot as a photo, video, GIF or file.",
            )
            .await?;
        }
        return Ok(());
    };
    if incoming.size.is_some_and(|s| s > MAX_DOWNLOAD) {
        reply(
            &bot,
            &msg,
            "That file is over 20 MB, which is as much as a bot can download.",
        )
        .await?;
        return Ok(());
    }
    match process(&bot, &msg, &incoming).await {
        Ok(Some(result)) => {
            tracing::info!(reason = %result.reason, score = result.score, "cropped")
        }
        Ok(None) => {
            reply(
                &bot,
                &msg,
                "Nothing to crop — that doesn't look like a screenshot.",
            )
            .await?
        }
        Err(err) => {
            tracing::error!(?err, "processing failed");
            reply(&bot, &msg, &format!("Couldn't process that one: {err}")).await?;
        }
    }
    Ok(())
}

async fn reply(bot: &Bot, msg: &Message, text: &str) -> Result<()> {
    bot.send_message(msg.chat.id, text)
        .reply_parameters(ReplyParameters::new(msg.id))
        .await?;
    Ok(())
}

/// Download, detect, crop and send. `None` when the detector found nothing.
async fn process(bot: &Bot, msg: &Message, inc: &Incoming) -> Result<Option<CropResult>> {
    let dir = tempdir_unique()?;
    let in_path = dir.join("in");
    let tg_file = bot
        .get_file(inc.file_id.clone())
        .await
        .context("get_file")?;
    let mut dst = tokio::fs::File::create(&in_path).await?;
    bot.download_file(&tg_file.path, &mut dst)
        .await
        .context("download_file")?;
    drop(dst);

    // Trust nothing about the declared type: sniff the bytes.
    let head = {
        let mut buf = [0u8; 16];
        let mut f = std::fs::File::open(&in_path)?;
        let n = std::io::Read::read(&mut f, &mut buf)?;
        buf[..n].to_vec()
    };
    let Some(kind) = sniff(&head) else {
        bail!("unsupported file type");
    };

    let result = match kind {
        Kind::Image(fmt) => crop_still(bot, msg, inc, &in_path, fmt).await?,
        Kind::Video => crop_clip(bot, msg, inc, &in_path, &dir).await?,
    };
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(result)
}

async fn crop_still(
    bot: &Bot,
    msg: &Message,
    inc: &Incoming,
    in_path: &Path,
    fmt: ImageFormat,
) -> Result<Option<CropResult>> {
    let in_path = in_path.to_owned();
    let (cropped, result) =
        tokio::task::spawn_blocking(move || autocrop::crop_image(&in_path, &Params::default()))
            .await?
            .context("decode image")?;
    if result.rect.is_none() {
        return Ok(None);
    }
    let bytes = cropped.encode(fmt.encoding()).context("encode crop")?;
    let name = out_name(&inc.name, fmt.ext());
    let file = InputFile::memory(bytes).file_name(name);
    let rp = ReplyParameters::new(msg.id);
    match inc.source {
        Source::Photo => {
            bot.send_photo(msg.chat.id, file)
                .reply_parameters(rp)
                .await?
        }
        _ => {
            bot.send_document(msg.chat.id, file)
                .reply_parameters(rp)
                .await?
        }
    };
    Ok(Some(result))
}

async fn crop_clip(
    bot: &Bot,
    msg: &Message,
    inc: &Incoming,
    in_path: &Path,
    dir: &Path,
) -> Result<Option<CropResult>> {
    let Some(result) = probe_clip(in_path, dir).await? else {
        return Ok(None);
    };
    let rect = result.rect.expect("probe returns Some only with a rect");
    let out_path = dir.join("out.mp4");
    let (w, h) = (rect.width() & !1, rect.height() & !1); // yuv420p needs even dims
    let ok = ffmpeg(&[
        "-i",
        &in_path.to_string_lossy(),
        "-vf",
        &format!("crop={w}:{h}:{}:{}", rect.x0, rect.y0),
        "-c:a",
        "copy",
        "-movflags",
        "+faststart",
        "-pix_fmt",
        "yuv420p",
        &out_path.to_string_lossy(),
    ])
    .await?;
    if !ok || !out_path.exists() {
        bail!("ffmpeg crop failed");
    }
    let file = InputFile::file(&out_path).file_name(out_name(&inc.name, "mp4"));
    let rp = ReplyParameters::new(msg.id);
    match inc.source {
        Source::Animation => {
            bot.send_animation(msg.chat.id, file)
                .reply_parameters(rp)
                .await?;
        }
        Source::Document => {
            bot.send_document(msg.chat.id, file)
                .reply_parameters(rp)
                .await?;
        }
        _ => {
            bot.send_video(msg.chat.id, file)
                .reply_parameters(rp)
                .supports_streaming(true)
                .await?;
        }
    }
    Ok(Some(result))
}

/// Run the detector on the first frame; when that is black or yields nothing,
/// retry 3 s in, then on the last frame for clips shorter than that.
async fn probe_clip(in_path: &Path, dir: &Path) -> Result<Option<CropResult>> {
    let frame = dir.join("frame.jpg");
    let input = in_path.to_string_lossy().into_owned();
    let frame_s = frame.to_string_lossy().into_owned();
    let seeks: [&[&str]; 3] = [&[], &["-ss", "3"], &["-sseof", "-0.5"]];
    for seek in seeks {
        let _ = tokio::fs::remove_file(&frame).await;
        let mut args: Vec<&str> = seek.to_vec();
        args.extend(["-i", &input, "-frames:v", "1", "-q:v", "2", &frame_s]);
        if !ffmpeg(&args).await? || !frame.exists() {
            continue; // seek past the end: no frame written
        }
        let img = RgbImage::load(&frame).context("decode probe frame")?;
        if is_black(&img) {
            tracing::info!(?seek, "probe frame is black");
            continue;
        }
        let result = find_crop(&img, &Params::default());
        if result.rect.is_some() {
            return Ok(Some(result));
        }
        tracing::info!(?seek, reason = %result.reason, "no crop in probe frame");
    }
    Ok(None)
}

fn is_black(img: &RgbImage) -> bool {
    let sum: f32 = img
        .pixels
        .iter()
        .map(|&p| autocrop::image::luminance(p))
        .sum();
    sum / img.pixels.len().max(1) as f32 <= BLACK_LUMA
}

async fn ffmpeg(args: &[&str]) -> Result<bool> {
    let out = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .await
        .context("spawn ffmpeg")?;
    if !out.status.success() {
        tracing::warn!(status = %out.status, stderr = %String::from_utf8_lossy(&out.stderr), "ffmpeg");
    }
    Ok(out.status.success())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageFormat {
    Jpeg,
    Png,
    Webp,
}

impl ImageFormat {
    fn encoding(self) -> Encoding {
        match self {
            Self::Jpeg => Encoding::Jpeg { quality: 90 },
            Self::Png => Encoding::Png,
            Self::Webp => Encoding::WebPLossless,
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Image(ImageFormat),
    Video,
}

/// Identify the container from its first bytes. GIF goes the video route so
/// animations keep every frame; BMP is decoded by the crate but re-encoded as PNG.
fn sniff(head: &[u8]) -> Option<Kind> {
    if head.starts_with(b"\xff\xd8\xff") {
        return Some(Kind::Image(ImageFormat::Jpeg));
    }
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(Kind::Image(ImageFormat::Png));
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return Some(Kind::Image(ImageFormat::Webp));
    }
    if head.starts_with(b"BM") {
        return Some(Kind::Image(ImageFormat::Png));
    }
    if head.starts_with(b"GIF8")
        || head.starts_with(b"\x1aE\xdf\xa3")
        || head.get(4..8) == Some(b"ftyp")
    {
        return Some(Kind::Video);
    }
    None
}

/// `shot.png` -> `shot_crop.<ext>`; no name -> `crop.<ext>`.
fn out_name(input: &str, ext: &str) -> String {
    let stem = input.rsplit_once('.').map_or(input, |(s, _)| s);
    if stem.is_empty() {
        format!("crop.{ext}")
    } else {
        format!("{stem}_crop.{ext}")
    }
}

fn tempdir_unique() -> Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("autocrop-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).context("create scratch dir")?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_by_magic() {
        assert_eq!(
            sniff(b"\xff\xd8\xff\xe0"),
            Some(Kind::Image(ImageFormat::Jpeg))
        );
        assert_eq!(
            sniff(b"\x89PNG\r\n\x1a\n...."),
            Some(Kind::Image(ImageFormat::Png))
        );
        assert_eq!(
            sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(Kind::Image(ImageFormat::Webp))
        );
        assert_eq!(sniff(b"\0\0\0\x18ftypisom"), Some(Kind::Video));
        assert_eq!(sniff(b"\x1aE\xdf\xa3\x01"), Some(Kind::Video));
        assert_eq!(sniff(b"GIF89a"), Some(Kind::Video));
        assert_eq!(sniff(b"%PDF-1.7"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn out_name_rules() {
        assert_eq!(out_name("shot.png", "png"), "shot_crop.png");
        assert_eq!(out_name("clip.mov", "mp4"), "clip_crop.mp4");
        assert_eq!(out_name("", "jpg"), "crop.jpg");
    }

    #[test]
    fn black_frame() {
        assert!(is_black(&RgbImage::solid(4, 4, [5, 5, 5])));
        assert!(!is_black(&RgbImage::solid(4, 4, [200, 200, 200])));
    }
}

#[cfg(test)]
mod ffmpeg_tests {
    use super::*;

    /// Synthetic clip: 1 s of black, then the autocrop-rs showcase screenshot.
    /// The probe must skip the black first frame and find the clip rectangle.
    #[tokio::test]
    async fn probe_skips_black_and_crops() {
        let shot = std::path::Path::new("tests/screenshot.jpg");
        let dir = tempdir_unique().unwrap();
        let clip = dir.join("clip.mp4");
        let ok = ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=600x1286:d=1",
            "-loop",
            "1",
            "-t",
            "5",
            "-i",
            &shot.to_string_lossy(),
            "-filter_complex",
            "[0:v][1:v]concat=n=2:v=1:a=0,format=yuv420p",
            "-r",
            "10",
            &clip.to_string_lossy(),
        ])
        .await
        .unwrap();
        assert!(ok);
        let result = probe_clip(&clip, &dir).await.unwrap().expect("crop found");
        let rect = result.rect.unwrap();
        assert_eq!((rect.x0, rect.x1), (0, 600));
        assert!(
            (400..430).contains(&rect.y0) && (740..760).contains(&rect.y1),
            "{rect:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
