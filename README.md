# autocrop-bot

Telegram bot that cuts screenshots down to the picture. Send it a photo, video,
GIF or file and it sends back only the content — no status bars, app chrome,
letterbox bars or meme text. Detection is
[autocrop](https://github.com/Nachtalb/autocrop-rs); images that don't look like
screenshots come back with a note instead of a crop.

## What it does

| input | how |
|---|---|
| photo, image file (JPEG, PNG, WebP, BMP) | detector on the image, crop re-encoded in the same format |
| video, GIF, video file (MP4/MOV, WebM/MKV, GIF) | detector on a probe frame, then `ffmpeg -vf crop` on the whole clip (audio copied) |

File types are sniffed from magic bytes, not the extension. The probe frame is
the first frame; if that is black or yields no crop the bot tries 3 s in, then
the last frame for shorter clips. Photos come back as photos, everything else
as the type it arrived in.

Public Bot API limits: 20 MB download, 50 MB upload.

Commands: `/start`, `/help`.

## Configuration

| Env var | Required | Notes |
|---|---|---|
| `TELOXIDE_TOKEN` | yes | BotFather token. |
| `RUST_LOG` | no | Defaults to `info,teloxide=info`. |

## Build & run

```bash
TELOXIDE_TOKEN=<token> cargo run
```

Needs `ffmpeg` on `PATH`. `cargo test` runs the ffmpeg path against a synthetic clip.

## Deploy

GitHub Actions builds `ghcr.io/nachtalb/autocrop-bot:latest` on push to `main`.
The image is `FROM scratch`: the static bot binary plus a static ffmpeg.
