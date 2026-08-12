{
  description = "Low-latency niri output streaming to Android";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        config = {
          allowUnfree = true;
          android_sdk.accept_license = true;
        };
      };
      android = pkgs.androidenv.composeAndroidPackages {
        platformVersions = [ "35" ];
        buildToolsVersions = [ "35.0.0" ];
        includeCmake = false;
        includeNDK = false;
      };
      androidSdk = android.androidsdk;
      guiPython = pkgs.python3.withPackages (pythonPackages: [
        pythonPackages.pycairo
        pythonPackages.pygobject3
      ]);
      guiTypelibPath = pkgs.lib.makeSearchPath "lib/girepository-1.0" (
        map pkgs.lib.getLib [
          pkgs.gdk-pixbuf
          pkgs.glib
          pkgs.gobject-introspection
          pkgs.graphene
          pkgs.gtk4
          pkgs.harfbuzz
          pkgs.pango
        ]
      );
      linuxPackages = with pkgs; [
        android-tools
        cargo
        clippy
        ffmpeg-headless
        jq
        rust-analyzer
        rustc
        rustfmt
        wf-recorder
      ];
    in
    {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "niri-android-monitor";
        version = "0.1.0";
        src = pkgs.lib.cleanSource ./.;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.makeWrapper ];
        postInstall = ''
          wrapProgram $out/bin/niri-android-monitor \
            --prefix PATH : ${pkgs.lib.makeBinPath [
              pkgs.android-tools
              pkgs.niri
              pkgs.wf-recorder
            ]}

          install -Dm755 gui/niri-android-monitor-settings.py \
            $out/libexec/niri-android-monitor-settings.py
          install -Dm644 gui/dev.niri.androidmonitor.Settings.desktop \
            $out/share/applications/dev.niri.androidmonitor.Settings.desktop
          makeWrapper ${guiPython}/bin/python \
            $out/bin/niri-android-monitor-settings \
            --add-flags $out/libexec/niri-android-monitor-settings.py \
            --prefix GI_TYPELIB_PATH : ${guiTypelibPath} \
            --prefix XDG_DATA_DIRS : ${pkgs.gtk4}/share
        '';
        meta.mainProgram = "niri-android-monitor";
      };

      apps.${system} = {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/niri-android-monitor";
          meta.description = "Stream a niri VKMS output to Android over USB";
        };
        gui = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/niri-android-monitor-settings";
          meta.description = "Configure the running niri Android monitor daemon";
        };
      };

      devShells.${system} = {
        default = pkgs.mkShell {
          packages = linuxPackages;
        };

        linux = pkgs.mkShell {
          packages = linuxPackages;
        };

        android = pkgs.mkShell {
          packages = with pkgs; [
            android-tools
            androidSdk
            gradle_8
            jdk17_headless
          ];

          ANDROID_HOME = "${androidSdk}/libexec/android-sdk";
          ANDROID_SDK_ROOT = "${androidSdk}/libexec/android-sdk";
          JAVA_HOME = "${pkgs.jdk17_headless}";
        };
      };

      nixosModules = {
        default = import ./nix/module.nix { inherit self; };
        monitor = self.nixosModules.default;
        vkms = import ./nix/vkms.nix;
      };
    };
}
