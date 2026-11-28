{
  lib,
  rustPlatform,
  pkg-config,
  makeWrapper,
  wayland,
  libdrm,
  libva,
  libgbm,
  libGL,
  ffmpeg,
  gst_all_1,
  version,
  withVaapi ? true,
}:

let
  gstPlugins = with gst_all_1; [
    gstreamer
    gst-plugins-base
    gst-plugins-good
    gst-plugins-bad
    gst-libav
  ];
in
rustPlatform.buildRustPackage {
  pname = "liuguang";
  inherit version;

  src = lib.cleanSource ./..;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    makeWrapper
  ];

  buildInputs = [
    wayland
    libdrm
    libgbm
    libGL # libEGL / libGLESv2（khronos-egl dynamic 运行期 dlopen）
    gst_all_1.gstreamer
  ]
  ++ gstPlugins
  ++ lib.optionals withVaapi [
    libva
    ffmpeg
  ];

  cargoBuildFlags = [
    "-p"
    "liuguang"
    "-p"
    "liuguangctl"
  ]
  ++ lib.optionals withVaapi [
    "--features"
    "liuguang/vaapi"
  ];
  # 上游测试依赖 debug_assert!（should_panic），须与 CI 一致在 debug 构型下运行。
  checkType = "debug";
  cargoTestFlags = [
    "-p"
    "liuguang-core"
    "-p"
    "liuguang-ipc"
  ];

  postInstall = ''
    install -Dm0644 packaging/liuguang.service $out/lib/systemd/system/liuguang.service
    install -Dm0644 packaging/liuguang-user.service $out/lib/systemd/user/liuguang.service
    install -Dm0644 packaging/liuguang.sysusers $out/lib/sysusers.d/liuguang.conf
    install -Dm0644 packaging/liuguang.tmpfiles $out/lib/tmpfiles.d/liuguang.conf
    substituteInPlace $out/lib/systemd/system/liuguang.service $out/lib/systemd/user/liuguang.service \
      --replace-quiet /usr/bin/ $out/bin/ \
      --replace-quiet /usr/share/doc/liuguang/ $out/share/doc/liuguang/
    install -Dm0644 config/example.toml $out/share/liuguang/config.example.toml
    install -Dm0644 README.md $out/share/doc/liuguang/README.md
    install -Dm0644 -t $out/share/doc/liuguang docs/*.md
  '';

  # GStreamer 插件与 EGL/GBM 都是运行期加载的，并设置 glibc malloc 默认阈值以抑制线程 arena 与未收缩堆导致的 RSS 膨胀。
  postFixup = ''
    wrapProgram $out/bin/liuguang \
      --set-default MALLOC_ARENA_MAX 2 \
      --set-default MALLOC_MMAP_THRESHOLD_ 131072 \
      --set-default MALLOC_TRIM_THRESHOLD_ 65536 \
      --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${
        lib.makeSearchPathOutput "lib" "lib/gstreamer-1.0" gstPlugins
      }" \
      --prefix LD_LIBRARY_PATH : "${
        lib.makeLibraryPath [
          libGL
          libgbm
        ]
      }"
  '';

  meta = {
    description = "High-performance Linux live wallpaper engine (Wayland layer-shell / DRM-KMS)";
    homepage = "https://github.com/suluoMISS/liuguang";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "liuguang";
  };
}
