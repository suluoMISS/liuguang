self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.liuguang;
  pkgsFor = self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  options.services.liuguang = {
    enable = lib.mkEnableOption "流光 Liuguang 动态壁纸";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgsFor.liuguang;
      description = "liuguang 守护进程与 liuguangctl。";
    };

    gui = {
      enable = lib.mkEnableOption "liuguang-gui 图形界面" // {
        default = true;
      };
      package = lib.mkOption {
        type = lib.types.package;
        default = pkgsFor.liuguang-gui;
      };
    };

    persona = lib.mkOption {
      type = lib.types.enum [
        "user"
        "system"
      ];
      default = "user";
      description = ''
        user：桌面 Wayland 壁纸层（用户单元，随 graphical-session 启动）。
        system：无头 DRM/KMS 装饰屏（系统单元）。同一台机器只启用其一。
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ] ++ lib.optional cfg.gui.enable cfg.gui.package;
    systemd.packages = [ cfg.package ];
    systemd.tmpfiles.packages = [ cfg.package ];
    hardware.graphics.enable = lib.mkDefault true;

    systemd.user.services.liuguang.wantedBy = lib.mkIf (cfg.persona == "user") [
      "graphical-session.target"
    ];

    environment.etc."liuguang/config.toml" = lib.mkIf (cfg.persona == "system") {
      source = "${cfg.package}/share/liuguang/config.example.toml";
    };
    users = lib.mkIf (cfg.persona == "system") {
      users.liuguang = {
        isSystemUser = true;
        group = "liuguang";
        extraGroups = [
          "video"
          "render"
        ];
      };
      groups.liuguang = { };
      groups.liuguang-ctl = { };
    };
    systemd.services.liuguang.wantedBy = lib.mkIf (cfg.persona == "system") [ "multi-user.target" ];
  };
}
