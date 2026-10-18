// ══════════════════════════════════════════════════════════════════════════════
// KWin ScreenShot2 DBus — direct inject into compositor.
//
// Two modes:
//   capture_output_dbus(conn, name) — capture a specific monitor by name.
//   capture_screen_dbus(conn)       — capture the active screen (single-tile fallback).
//
// Protocol: SCM_RIGHTS passes fd to KWin, which writes raw QImage pixels;
// the reply's format and stride metadata define their layout.
// Authorization: silent → interactive retry on NoAuthorized.
// ══════════════════════════════════════════════════════════════════════════════

use anyhow::{Result, anyhow};
use std::fs::File;
use std::time::{Duration, Instant};

use crate::core::terminal::{log_step, log_warn, log_info};
use super::{ScreenCapture, rgba_to_capture};

/// **The Ghost DBus Call**
/// Fires directly at KWin. Works only if our app is authorized,
/// which requires a correct .desktop file with `X-KDE-DBUS-Restricted-Interfaces`.
/// Has a fallback matrix: first tries silently, and if NoAuthorized is returned,
/// retries with `interactive=true`, requesting permission from the compositor.
pub fn capture_screen_dbus(conn: &zbus::blocking::Connection) -> Result<ScreenCapture> {
    // 1. Attempt silent capture (fast, <10ms, if authorized)
    match perform_kwin_capture(conn, false, None) {
        Ok(img) => Ok(img),
        Err(e) => {
            let err_str = e.to_string();
            // 2. If KWin says "Not authorized", request permission interactively
            if err_str.contains("NoAuthorized") {
                log_warn("KWin Authorization missing. Retrying with interactive prompt...");
                log_info("Action Required: Please click 'Allow' and check 'Remember' in the KWin dialog!");

                // 3. Attempt interactive capture (shows dialog if user hasn't saved the choice)
                match perform_kwin_capture(conn, true, None) {
                    Ok(img) => {
                        log_info("Interactive authorization granted! Next time it will be silent.");
                        Ok(img)
                    }
                    Err(e2) => Err(anyhow!("KWin Interactive Capture failed: {}", e2)),
                }
            } else {
                Err(e)
            }
        }
    }
}

/// Capture a specific monitor by name (KWin ScreenShot2 → CaptureScreen).
/// For Plasma multi-monitor: called separately for each output.
pub fn capture_output_dbus(
    conn: &zbus::blocking::Connection,
    screen_name: &str,
) -> Result<ScreenCapture> {
    perform_kwin_capture(conn, false, Some(screen_name))
}

fn perform_kwin_capture(
    conn: &zbus::blocking::Connection,
    interactive: bool,
    // Some(name) → CaptureScreen(name)  /  None → CaptureActiveScreen
    screen_name: Option<&str>,
) -> Result<ScreenCapture> {
    use std::collections::HashMap;
    use std::os::unix::io::AsFd;
    use tempfile::NamedTempFile;
    use zbus::zvariant::OwnedValue;

    let t0 = Instant::now();

    // 1. FD setup (File Descriptor Matrix)
    // Create a Unix temp file. KWin will dump raw XRGB pixels directly into it.
    // No PNG encoding — raw bytes only.
    let temp_file = NamedTempFile::new()?;

    // Convert file to descriptor. SCM_RIGHTS protocol clones it into the compositor process.
    let fd = zbus::zvariant::Fd::from(temp_file.as_fd());

    let mut options: HashMap<String, OwnedValue> = HashMap::new();
    options.insert("interactive".to_string(), OwnedValue::from(interactive));
    options.insert("native-resolution".to_string(), OwnedValue::from(true));

    let t1 = Instant::now();

    // 2. Injection (The Trigger)
    // Make the call. At this point the kernel (via SCM_RIGHTS) secretly passes our `fd`
    // directly into KWin compositor's internal file descriptor table.
    let reply = if let Some(name) = screen_name {
        conn.call_method(
            Some("org.kde.KWin"),
            "/org/kde/KWin/ScreenShot2",
            Some("org.kde.KWin.ScreenShot2"),
            "CaptureScreen",
            &(name, options, fd),
        )?
    } else {
        conn.call_method(
            Some("org.kde.KWin"),
            "/org/kde/KWin/ScreenShot2",
            Some("org.kde.KWin.ScreenShot2"),
            "CaptureActiveScreen",
            &(options, fd),
        )?
    };

    let t2 = Instant::now();
    log_step("Capture", &format!("KWin DBus: {}ms", (t2 - t1).as_millis()));

    let metadata: HashMap<String, OwnedValue> = reply.body().deserialize()?;
    let width = metadata
        .get("width")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    let height = metadata
        .get("height")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    let reply_type = metadata
        .get("type")
        .and_then(|v| <&str>::try_from(v).ok())
        .unwrap_or("unknown");
    let raw_format = metadata.get("format").and_then(|v| u32::try_from(v).ok());
    let raw_stride = metadata.get("stride").and_then(|v| u32::try_from(v).ok());

    if width == 0 || height == 0 {
        return Err(anyhow!("KWin capture: invalid dimensions {}x{}", width, height));
    }
    let expected_size = if reply_type == "raw" {
        raw_stride.ok_or_else(|| anyhow!("KWin raw capture omitted stride metadata"))? as u64
            * height as u64
    } else {
        width as u64 * height as u64 * 4
    };
    // Pre-allocate: we know the exact size (14MB for 2560×1440).
    // Without this, Vec grows through 3-4 reallocs (0→4K→32K→...→14MB).
    let mut bytes = Vec::with_capacity(expected_size as usize);
    let mut poll_count = 0u32;
    let t3 = Instant::now();
    let mut file = File::open(temp_file.path())?;

    use std::io::{Read, Seek, SeekFrom};

    while t3.elapsed() < Duration::from_millis(500) {
        // seek(End) is cheaper than metadata().len(): one lseek vs fstat
        let current_size = file.seek(SeekFrom::End(0)).unwrap_or(0);
        if reply_type == "raw" && current_size >= expected_size {
            file.seek(SeekFrom::Start(0))?;
            let t_read = Instant::now();
            file.read_to_end(&mut bytes)?;
            log_step("Capture", &format!("File read: {}ms ({}MB)", t_read.elapsed().as_millis(), bytes.len() / 1_000_000));
            break;
        } else if reply_type != "raw" && current_size > 0 {
            file.seek(SeekFrom::Start(0))?;
            file.read_to_end(&mut bytes)?;
            break;
        }
        poll_count += 1;
        std::thread::sleep(Duration::from_millis(1));
    }

    let t4 = Instant::now();
    log_step("Capture", &format!("Polling: {}ms ({} polls)", (t4 - t3).as_millis(), poll_count));

    if bytes.is_empty() {
        return Err(anyhow!("DBus capture resulted in empty or incomplete data"));
    }

    if reply_type == "raw" && width > 0 && height > 0 {
        if bytes.len() >= expected_size as usize {
            let format = raw_format
                .ok_or_else(|| anyhow!("KWin raw capture omitted QImage format metadata"))?;
            let stride = raw_stride.unwrap();
            bytes.truncate(expected_size as usize);
            let capture = decode_raw_qimage(&bytes, width, height, stride, format)?;
            log_step(
                "Capture",
                &format!(
                    "Total: {}ms ({}x{}, type={}, format={}, stride={})",
                    t0.elapsed().as_millis(),
                    width,
                    height,
                    reply_type,
                    format,
                    stride
                ),
            );
            return Ok(capture);
        } else {
            log_warn(&format!("Raw capture size mismatch: expected >= {}, got {}", expected_size, bytes.len()));
        }
    }

    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let dyn_img = reader.decode()?;
    Ok(rgba_to_capture(&dyn_img.to_rgba8()))
}

fn decode_raw_qimage(
    bytes: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
) -> Result<ScreenCapture> {
    const RGB32: u32 = 4;
    const ARGB32: u32 = 5;
    const ARGB32_PREMULTIPLIED: u32 = 6;
    const RGBX8888: u32 = 16;
    const RGBA8888: u32 = 17;
    const RGBA8888_PREMULTIPLIED: u32 = 18;

    if !matches!(
        format,
        RGB32 | ARGB32 | ARGB32_PREMULTIPLIED | RGBX8888 | RGBA8888 | RGBA8888_PREMULTIPLIED
    ) {
        anyhow::bail!("unsupported KWin raw QImage format {format}");
    }

    let row_bytes = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| anyhow!("KWin raw capture row size overflow"))?;
    if (stride as usize) < row_bytes {
        anyhow::bail!("KWin raw capture stride {stride} is smaller than row size {row_bytes}");
    }
    let required_size = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("KWin raw capture size overflow"))?;
    if bytes.len() < required_size {
        anyhow::bail!(
            "KWin raw capture is truncated: expected {required_size} bytes, got {}",
            bytes.len()
        );
    }

    let pixel_count = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("KWin raw capture pixel count overflow"))?;
    let mut xrgb_buffer = Vec::with_capacity(pixel_count);

    for y in 0..height as usize {
        let start = y * stride as usize;
        for pixel in bytes[start..start + row_bytes].chunks_exact(4) {
            let (mut red, mut green, mut blue, alpha) = match format {
                RGB32 | ARGB32 | ARGB32_PREMULTIPLIED => {
                    let value = u32::from_ne_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
                    (
                        ((value >> 16) & 0xff) as u8,
                        ((value >> 8) & 0xff) as u8,
                        (value & 0xff) as u8,
                        (value >> 24) as u8,
                    )
                }
                RGBX8888 => (pixel[0], pixel[1], pixel[2], 0xff),
                RGBA8888 | RGBA8888_PREMULTIPLIED => (pixel[0], pixel[1], pixel[2], pixel[3]),
                _ => unreachable!(),
            };

            if matches!(format, ARGB32_PREMULTIPLIED | RGBA8888_PREMULTIPLIED)
                && alpha != 0
                && alpha != 0xff
            {
                red = ((red as u32 * 0xff + alpha as u32 / 2) / alpha as u32).min(0xff) as u8;
                green = ((green as u32 * 0xff + alpha as u32 / 2) / alpha as u32).min(0xff) as u8;
                blue = ((blue as u32 * 0xff + alpha as u32 / 2) / alpha as u32).min(0xff) as u8;
            }

            xrgb_buffer.push(((red as u32) << 16) | ((green as u32) << 8) | blue as u32);
        }
    }

    Ok(ScreenCapture {
        xrgb_buffer,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::decode_raw_qimage;

    #[test]
    fn decodes_rgbx8888_channels_and_row_stride() {
        let bytes = [
            0x12, 0x34, 0x56, 0xff, 0xaa, 0xaa, 0xaa, 0xaa, 0xa0, 0xb0, 0xc0, 0xff, 0xbb, 0xbb,
            0xbb, 0xbb,
        ];

        let capture = decode_raw_qimage(&bytes, 1, 2, 8, 16).unwrap();

        assert_eq!(capture.xrgb_buffer, vec![0x123456, 0xa0b0c0]);
    }

    #[test]
    fn decodes_legacy_argb32_premultiplied_words() {
        let bytes = 0xff12_3456u32.to_ne_bytes();

        let capture = decode_raw_qimage(&bytes, 1, 1, 4, 6).unwrap();

        assert_eq!(capture.xrgb_buffer, vec![0x123456]);
    }
}
