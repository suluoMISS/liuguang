//! 构建脚本：定位 libva / libva-drm / libdrm / libav* 并探测 FFI 需要的字段偏移。
//!
//! 为什么要在构建期用 C 探测偏移，而不是在 Rust 里手写结构体布局：
//! - 读多写少的结构（`AVFrame`/`AVStream`/`AVCodecParameters`/`AVHWDeviceContext`）
//!   用手写 `#[repr(C)]` 前缀是安全的，并且下面会用 `offset_of!` 做**编译期断言**；
//! - 但 `AVCodecContext` 要**写**入 `hw_device_ctx` 指针，字段多、版本间会移动。
//!   写错偏移不会报错，只会让解码器悄悄走软解（甚至崩溃）。
//!   因此用系统头文件里的 `offsetof` 算出来，作为环境变量传给 Rust 侧。
//!
//! 构建机与运行机必须是同一套 ffmpeg/libva（本 crate 是 `feature = "vaapi"` 时才构建的
//! 可选组件，打包环境不启用它，见 `packaging/liuguang.spec`）。
use std::process::Command;

/// 需要 pkg-config 定位的模块，以及要链接的库。
const PKG_MODULES: [&str; 4] = ["libva", "libavformat", "libavcodec", "libavutil"];
const LINK_LIBS: [&str; 6] = ["va", "va-drm", "drm", "avformat", "avcodec", "avutil"];

/// 探测程序：打印 `<name>=<offset>`，一行一个。
const PROBE_SOURCE: &str = r#"
#include <stddef.h>
#include <stdio.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/buffer.h>
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>

int main(void) {
    printf("avcodec_ctx_hw_device_ctx=%zu\n", offsetof(AVCodecContext, hw_device_ctx));
    printf("avcodec_ctx_pix_fmt=%zu\n", offsetof(AVCodecContext, pix_fmt));
    printf("avframe_data=%zu\n", offsetof(AVFrame, data));
    printf("avframe_linesize=%zu\n", offsetof(AVFrame, linesize));
    printf("avframe_width=%zu\n", offsetof(AVFrame, width));
    printf("avframe_height=%zu\n", offsetof(AVFrame, height));
    printf("avframe_format=%zu\n", offsetof(AVFrame, format));
    printf("avframe_pts=%zu\n", offsetof(AVFrame, pts));
    printf("avformat_ctx_nb_streams=%zu\n", offsetof(AVFormatContext, nb_streams));
    printf("avformat_ctx_streams=%zu\n", offsetof(AVFormatContext, streams));
    printf("avstream_codecpar=%zu\n", offsetof(AVStream, codecpar));
    printf("avstream_time_base=%zu\n", offsetof(AVStream, time_base));
    printf("avstream_avg_frame_rate=%zu\n", offsetof(AVStream, avg_frame_rate));
    printf("avstream_r_frame_rate=%zu\n", offsetof(AVStream, r_frame_rate));
    printf("avstream_avg_frame_rate=%zu\n", offsetof(AVStream, avg_frame_rate));
    printf("avstream_r_frame_rate=%zu\n", offsetof(AVStream, r_frame_rate));
    printf("avpacket_stream_index=%zu\n", offsetof(AVPacket, stream_index));
    printf("avcodecpar_codec_type=%zu\n", offsetof(AVCodecParameters, codec_type));
    printf("avcodecpar_width=%zu\n", offsetof(AVCodecParameters, width));
    printf("avcodecpar_height=%zu\n", offsetof(AVCodecParameters, height));
    /* 容器声明的像素格式：10bit 素材在这里就能认出来（不必等解码表面导出失败）。 */
    printf("avcodecpar_format=%zu\n", offsetof(AVCodecParameters, format));
    printf("avbuffer_ref_data=%zu\n", offsetof(AVBufferRef, data));
    printf("avhwdevice_ctx_hwctx=%zu\n", offsetof(AVHWDeviceContext, hwctx));
    printf("avvaapi_device_ctx_display=%zu\n", offsetof(AVVAAPIDeviceContext, display));
    printf("av_pix_fmt_vaapi=%d\n", (int)AV_PIX_FMT_VAAPI);
    printf("av_hwdevice_type_vaapi=%d\n", (int)AV_HWDEVICE_TYPE_VAAPI);
    /* 解码器选择（AV1 默认解码器 libdav1d 没有硬件配置，必须按 hw_config 重挑）。 */
    printf("avcodec_hwconfig_pix_fmt=%zu\n", offsetof(AVCodecHWConfig, pix_fmt));
    printf("avcodec_hwconfig_methods=%zu\n", offsetof(AVCodecHWConfig, methods));
    printf("avcodec_hwconfig_device_type=%zu\n", offsetof(AVCodecHWConfig, device_type));
    printf("avcodec_id=%zu\n", offsetof(AVCodec, id));
    printf("avcodec_name=%zu\n", offsetof(AVCodec, name));
    printf("avcodec_hw_config_method_hw_device_ctx=%d\n",
           (int)AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX);
    /* 10bit 判定：这两种 10bit 格式是 P010/yuv420p10le 素材的直接证据。 */
    printf("av_pix_fmt_p010le=%d\n", (int)AV_PIX_FMT_P010LE);
    printf("av_pix_fmt_yuv420p10le=%d\n", (int)AV_PIX_FMT_YUV420P10LE);
    /* 编码 id 不手抄枚举值：按头文件读。 */
    printf("av_codec_id_h264=%d\n", (int)AV_CODEC_ID_H264);
    printf("av_codec_id_hevc=%d\n", (int)AV_CODEC_ID_HEVC);
    printf("av_codec_id_av1=%d\n", (int)AV_CODEC_ID_AV1);
    return 0;
}
"#;

fn fail(message: &str) -> ! {
    panic!(
        "{message}\n\n这个 crate 只在 feature = \"vaapi\" 时构建。若在打包/CI 环境里看到本条，\
         说明该环境缺少 libva 或 ffmpeg 的开发包（Fedora：libva-devel ffmpeg-devel；\
         Arch：libva libva-utils ffmpeg）。生产包不启用该 feature。"
    );
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    for module in PKG_MODULES {
        let status = Command::new("pkg-config")
            .args(["--exists", module])
            .status()
            .unwrap_or_else(|e| fail(&format!("无法运行 pkg-config：{e}")));
        if !status.success() {
            fail(&format!("pkg-config 找不到 {module}"));
        }
    }

    for lib in LINK_LIBS {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }

    // 把探测程序写到 OUT_DIR 再编译执行；用 pkg-config 给出的 cflags 保证头文件路径一致。
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|e| fail(&format!("缺少 OUT_DIR：{e}")));
    let out = std::path::Path::new(&out_dir);
    let src = out.join("layout_probe.c");
    let exe = out.join("layout_probe");
    std::fs::write(&src, PROBE_SOURCE).unwrap_or_else(|e| fail(&format!("写入探测程序失败：{e}")));

    let cflags = Command::new("pkg-config")
        .args(["--cflags"])
        .args(PKG_MODULES)
        .output()
        .unwrap_or_else(|e| fail(&format!("pkg-config --cflags 失败：{e}")));
    let cflags = String::from_utf8_lossy(&cflags.stdout);
    // pkg-config 输出是按空白分隔的；这里不追求 shell 精确，直接按空白切分即可。
    let mut cc = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".to_string()));
    cc.arg("-o").arg(&exe).arg(&src);
    for flag in cflags.split_whitespace() {
        cc.arg(flag);
    }
    let compiled = cc.status().unwrap_or_else(|e| {
        fail(&format!(
            "无法调用 C 编译器（CC={:?}）：{e}",
            cc.get_program()
        ))
    });
    if !compiled.success() {
        fail("布局探测程序编译失败（libavcodec/libavutil/libavformat 头文件是否完整？）");
    }

    let output = Command::new(&exe)
        .output()
        .unwrap_or_else(|e| fail(&format!("布局探测程序无法运行：{e}")));
    if !output.status.success() {
        fail("布局探测程序运行失败");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut seen = Vec::new();
    for line in stdout.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().to_ascii_uppercase();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        println!("cargo:rustc-env={name}={value}");
        seen.push(name);
    }
    for required in [
        "AVCODEC_CTX_HW_DEVICE_CTX",
        "AVCODEC_HWCONFIG_PIX_FMT",
        "AVCODEC_HWCONFIG_METHODS",
        "AVCODEC_ID",
        "AVCODECPAR_FORMAT",
        "AV_PIX_FMT_P010LE",
        "AV_PIX_FMT_YUV420P10LE",
        "AV_CODEC_ID_AV1",
        "AVFRAME_DATA",
        "AVFORMAT_CTX_NB_STREAMS",
        "AVSTREAM_CODECPAR",
        "AVHWDEVICE_CTX_HWCTX",
        "AVVAAPI_DEVICE_CTX_DISPLAY",
        "AV_PIX_FMT_VAAPI",
        "AV_HWDEVICE_TYPE_VAAPI",
    ] {
        if !seen.iter().any(|name| name == required) {
            fail(&format!("布局探测没有产出 {required}"));
        }
    }
}
