{
  lib,
  rustPlatform,
  pkg-config,
  wrapGAppsHook4,
  gtk4,
  libadwaita,
  gdk-pixbuf,
  version,
}:

rustPlatform.buildRustPackage {
  pname = "liuguang-gui";
  inherit version;

  # GUI 是独立 workspace，但依赖 ../../crates，所以源码取整棵树。
  src = lib.cleanSource ./..;
  cargoRoot = "apps/liuguang-gui";
  buildAndTestSubdir = "apps/liuguang-gui";
  cargoLock.lockFile = ../apps/liuguang-gui/Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
  ];
  buildInputs = [
    gtk4
    libadwaita
    gdk-pixbuf
  ];

  doCheck = false;

  postInstall = ''
    install -Dm0644 packaging/liuguang-gui.desktop $out/share/applications/liuguang-gui.desktop
    install -Dm0644 packaging/icons/liuguang.svg $out/share/icons/hicolor/scalable/apps/liuguang.svg
    for size in 16 24 32 48 64 128 256 512; do
      install -Dm0644 packaging/icons/liuguang-$size.png \
        $out/share/icons/hicolor/''${size}x''${size}/apps/liuguang.png
    done
  '';

  meta = {
    description = "GTK4 control panel for the Liuguang live wallpaper daemon";
    homepage = "https://github.com/suluoMISS/liuguang";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "liuguang-gui";
  };
}
