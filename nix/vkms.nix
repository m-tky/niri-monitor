{ config, lib, pkgs, ... }:

let
  cfg = config.services.niri-android-vkms;
  helper = pkgs.writeShellScript "niri-android-vkms" (builtins.readFile ../scripts/vkms-configfs.sh);
in
{
  options.services.niri-android-vkms = {
    enable = lib.mkEnableOption "a VKMS connector for niri Android monitor";
    instance = lib.mkOption {
      type = lib.types.str;
      default = "niri-android";
      description = "VKMS configfs instance name.";
    };
  };

  config = lib.mkIf cfg.enable {
    boot.kernelModules = [ "vkms" ];
    boot.extraModprobeConfig = "options vkms create_default_dev=0";

    systemd.services.niri-android-vkms = {
      description = "Create the niri Android VKMS connector";
      wantedBy = [ "multi-user.target" ];
      after = [ "systemd-modules-load.service" "sys-kernel-config.mount" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${helper} setup";
        ExecStop = "${helper} teardown";
      };
      environment.VKMS_INSTANCE = cfg.instance;
      path = [ pkgs.coreutils pkgs.kmod ];
    };
  };
}
