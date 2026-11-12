//! Native video presentation policy. This never changes FFmpeg/NVENC encoding.
use std::path::Path;

/// WebKit defaults its download-buffer byte watermark to only 100 KiB. Since
/// GStreamer takes the maximum of byte/time percentages, a time-only setting
/// cannot build a useful cushion for high-bitrate Wi-Fi recordings.
/// https://github.com/WebKit/WebKit/blob/main/Source/WebCore/platform/graphics/gstreamer/MediaPlayerPrivateGStreamer.cpp
pub fn download_environment() -> [(&'static str, &'static str); 2] {
    [
        ("WPE_SHELL_MEDIA_DISK_CACHE_SIZE_BYTES", "8388608"),
        ("WPE_SHELL_MEDIA_DISK_CACHE_SIZE_NSEC", "2000000000"),
    ]
}

pub fn nvidia_gpu_at(root: &Path) -> bool {
    // The proc entry also covers machines where the NVIDIA render node is not
    // visible in the app's device namespace (including hybrid configurations).
    if root.join("proc/driver/nvidia/version").is_file() {
        return true;
    }
    std::fs::read_dir(root.join("sys/class/drm"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| {
            std::fs::read_to_string(entry.path().join("device/vendor"))
                .is_ok_and(|vendor| vendor.trim().eq_ignore_ascii_case("0x10de"))
        })
}

pub fn nvidia_gpu() -> bool {
    nvidia_gpu_at(Path::new("/"))
        || [
            "LIBVA_DRIVER_NAME",
            "__GLX_VENDOR_LIBRARY_NAME",
            "GBM_BACKEND",
        ]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .any(|value| value.to_ascii_lowercase().contains("nvidia"))
}

/// Build before GTK/WebKit starts so its video subprocesses inherit the same
/// settings. Keep the policy pure so it can be exercised without an NVIDIA GPU.
pub fn environment(
    hardware_decode: bool,
    nvidia: bool,
    ranks: &str,
) -> Vec<(&'static str, String)> {
    let mut result = Vec::new();
    if nvidia {
        // WebKit's NV12 DMA-BUF/GL import can display green instead of the
        // decoded picture. Use its system-memory video sink and CPU decoder
        // together: disabling only the decoder still leaves the unsafe sink.
        // https://bugs.webkit.org/show_bug.cgi?id=260654
        result.push(("WEBKIT_GST_DMABUF_SINK_DISABLED", "1".into()));
        result.push(("WEBKIT_GST_DISABLE_GL_SINK", "1".into()));
    }
    if !hardware_decode || nvidia {
        let mut ranks = ranks.to_owned();
        let mut append = |feature: &str, rank: &str| {
            if !ranks.is_empty() {
                ranks.push(',');
            }
            ranks.push_str(feature);
            ranks.push(':');
            ranks.push_str(rank);
        };
        for decoder in [
            "avdec_h264",
            "avdec_h265",
            "avdec_av1",
            "dav1ddec",
            "av1dec",
            "avdec_vp8",
            "avdec_vp9",
        ] {
            append(decoder, "MAX");
        }
        if nvidia {
            for decoder in [
                "vah264dec",
                "vah265dec",
                "vaav1dec",
                "vavp8dec",
                "vavp9dec",
                "vaapih264dec",
                "vaapih265dec",
                "vaapiav1dec",
                "vaapivp8dec",
                "vaapivp9dec",
                "nvh264dec",
                "nvh265dec",
                "nvav1dec",
                "nvvp8dec",
                "nvvp9dec",
                "nvh264sldec",
                "nvh265sldec",
                "nvav1sldec",
                "nvvp8sldec",
                "nvvp9sldec",
            ] {
                append(decoder, "0");
            }
        }
        result.push(("GST_PLUGIN_FEATURE_RANK", ranks));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_nvidia_including_hybrid_and_proc_only_devices() {
        let root = tempfile::tempdir().unwrap();
        assert!(!nvidia_gpu_at(root.path()));
        let amd = root.path().join("sys/class/drm/card0/device");
        std::fs::create_dir_all(&amd).unwrap();
        std::fs::write(amd.join("vendor"), "0x1002\n").unwrap();
        assert!(!nvidia_gpu_at(root.path()));
        let nvidia = root.path().join("sys/class/drm/renderD129/device");
        std::fs::create_dir_all(&nvidia).unwrap();
        std::fs::write(nvidia.join("vendor"), "0x10de\n").unwrap();
        assert!(nvidia_gpu_at(root.path()));
        std::fs::remove_dir_all(root.path().join("sys")).unwrap();
        let proc = root.path().join("proc/driver/nvidia");
        std::fs::create_dir_all(&proc).unwrap();
        std::fs::write(proc.join("version"), "NVIDIA kernel driver").unwrap();
        assert!(nvidia_gpu_at(root.path()));
    }
    #[test]
    fn nvidia_uses_cpu_frames_and_system_memory_presentation_despite_hardware_preference() {
        let policy: std::collections::HashMap<_, _> =
            environment(true, true, "nvh264dec:MAX,custom:123")
                .into_iter()
                .collect();
        assert_eq!(policy["WEBKIT_GST_DMABUF_SINK_DISABLED"], "1");
        assert_eq!(policy["WEBKIT_GST_DISABLE_GL_SINK"], "1");
        assert!(policy["GST_PLUGIN_FEATURE_RANK"].starts_with("nvh264dec:MAX,custom:123,"));
        assert!(policy["GST_PLUGIN_FEATURE_RANK"].contains("avdec_h264:MAX"));
        assert!(policy["GST_PLUGIN_FEATURE_RANK"].contains("nvh264dec:0"));
        assert!(policy["GST_PLUGIN_FEATURE_RANK"].contains("vah264dec:0"));
        assert!(
            policy
                .keys()
                .all(|key| key.starts_with("WEBKIT_GST_") || *key == "GST_PLUGIN_FEATURE_RANK")
        );
        assert!(!policy.values().any(|value| value.contains("nvenc")));
    }
    #[test]
    fn other_gpus_keep_hardware_playback_and_software_override() {
        assert!(environment(true, false, "custom:123").is_empty());
        let software = environment(false, false, "custom:123");
        assert_eq!(software.len(), 1);
        assert_eq!(software[0].0, "GST_PLUGIN_FEATURE_RANK");
        assert!(software[0].1.starts_with("custom:123,"));
        assert!(software[0].1.contains("avdec_h264:MAX"));
    }
}
