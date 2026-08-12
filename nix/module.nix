{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.niri-android-monitor;
  inherit (lib) mkEnableOption mkIf mkOption optional optionals types;
  executable = lib.getExe cfg.package;
  arguments = [
    "--output" cfg.output
    "--width" (toString cfg.width)
    "--height" (toString cfg.height)
    "--fps" (toString cfg.fps)
    "--encoder" cfg.encoder
    "--render-node" cfg.renderNode
  ]
  ++ optional (cfg.adbSerial != null) "--adb-serial"
  ++ optional (cfg.adbSerial != null) cfg.adbSerial
  ++ optionals (!cfg.touch.enable) [ "--no-touch" ];
  launcher = pkgs.writeShellScript "niri-android-monitor-launch" ''
    # A graphical user service does not consistently inherit NIRI_SOCKET.
    # Wait for niri's IPC socket so the initial `output off` cannot race niri.
    if test -z "''${NIRI_SOCKET:-}"; then
      attempt=0
      while test "$attempt" -lt 100; do
        for socket in "''${XDG_RUNTIME_DIR}"/niri.*.sock; do
          if test -S "$socket"; then
            export NIRI_SOCKET="$socket"
            break 2
          fi
        done
        attempt=$((attempt + 1))
        sleep 0.1
      done
    fi
    if test -z "''${NIRI_SOCKET:-}"; then
      echo "niri IPC socket did not appear" >&2
      exit 1
    fi
    exec ${lib.escapeShellArgs ([ executable ] ++ arguments)}
  '';
in
{
  imports = [ ./vkms.nix ];

  options.services.niri-android-monitor = {
    enable = mkEnableOption "the USB niri monitor daemon and its VKMS output";

    user = mkOption {
      type = types.str;
      description = "Desktop user that owns the niri session.";
    };

    package = mkOption {
      type = types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "niri-android-monitor package from this flake";
      description = "Daemon package to run.";
    };

    output = mkOption {
      type = types.str;
      default = "Virtual-1";
      description = "niri output name assigned to the VKMS connector.";
    };

    width = mkOption {
      type = types.ints.positive;
      default = 1920;
      description = "Stream and virtual output width.";
    };

    height = mkOption {
      type = types.ints.positive;
      default = 1080;
      description = "Stream and virtual output height.";
    };

    fps = mkOption {
      type = types.ints.positive;
      default = 60;
      description = "Maximum output and capture frame rate; static frames are not duplicated.";
    };

    adbSerial = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Optional Android ADB serial when more than one device is attached.";
    };

    encoder = mkOption {
      type = types.enum [ "auto" "vaapi" "x264" ];
      default = "auto";
      description = "H.264 encoder selection.";
    };

    renderNode = mkOption {
      type = types.str;
      default = "/dev/dri/renderD128";
      description = "DRM render node used by VA-API.";
    };

    touch.enable = mkOption {
      type = types.bool;
      default = true;
      description = "Forward Android touch as absolute mouse input through ydotoold.";
    };
  };

  config = mkIf cfg.enable {
    services.niri-android-vkms.enable = true;
    environment.systemPackages = [ cfg.package ];

    programs.ydotool.enable = cfg.touch.enable;
    users.users.${cfg.user}.extraGroups = optional cfg.touch.enable config.programs.ydotool.group;

    systemd.user.services.niri-android-monitor = {
      description = "Low-latency Android monitor for niri";
      wantedBy = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];
      unitConfig.ConditionUser = cfg.user;
      serviceConfig = {
        ExecStart = launcher;
        Restart = "on-failure";
        RestartSec = 1;
      };
    };
  };
}
