# niri Android monitor

USB-connected Android deviceを、niriの低遅延サブモニターとして使う実験実装です。

```text
VKMS / niri output → wlr-screencopy DMA-BUF → VA-API H.264
                 → ADB reverse → MediaCodec → SurfaceView
Android touch → 同じTCP接続 → ydotoold → uinput → niri
```

niri本体への変更と物理ダミープラグは不要です。VKMS connectorはカーネルに登録したままにし、
Androidアプリが接続している間だけniri outputを有効にします。アプリ終了、USB切断、デーモンの
SIGINT/SIGTERM、エンコーダ異常終了のいずれでもoutputをOFFへ戻します。

## 省電力と遅延の方針

`--fps`は最大フレームレートです。キャプチャはdamage駆動のVFRで、静止画を指定fpsまで複製
しません。したがって、動きが止まるとエンコーダとUSB転送もほぼ停止し、次のdamageは直ちに
処理されます。LCDのリフレッシュレートを明示的に切り替えないため、再び動いた時のモード切替
待ちも入りません。

この実機では1920x1080@60の静止時に約1 fps（compositor側の更新を含む）、`wf-recorder`
約0.5% CPUでした。動きがある時だけ必要なfpsまで上がります。

`auto` encoderはまずVA-API + DMA-BUFを試し、起動直後に失敗した場合はx264へフォールバック
します。明示的に固定する場合は`--encoder vaapi`または`--encoder x264`を使います。

## NixOSで常用する

flakeのNixOS moduleはVKMS、ydotoold、ユーザーサービスをまとめて設定します。

```nix
{
  inputs.niri-android-monitor.url = "path:/home/user/Code/niri-android-monitor";

  outputs = inputs@{ nixpkgs, niri-android-monitor, ... }: {
    # nixosSystemのmodulesに追加
    # niri-android-monitor.nixosModules.default
  };
}
```

ホスト設定側:

```nix
{
  imports = [ inputs.niri-android-monitor.nixosModules.default ];

  services.niri-android-monitor = {
    enable = true;
    user = "user";
    adbSerial = "f2ccba87"; # 1台だけならnullでもよい
    output = "Virtual-1";
    width = 1920;
    height = 1080;
    fps = 60;
    encoder = "auto";
    renderNode = "/dev/dri/renderD128";
    touch.enable = true;
  };
}
```

rebuild後は、ydotool groupを反映するため一度ログアウト・ログインしてください。ユーザーサービスは
niri IPC socketを待ってから起動し、最初に`Virtual-1`をOFFにします。Androidアプリが閉じている
時の`niri msg -j outputs`では`current_mode`と`logical`がともに`null`になります。

VKMSだけを管理したい場合は、従来どおり`nixosModules.vkms`と
`services.niri-android-vkms.enable = true`も利用できます。

## 設定GUI

NixOS moduleを有効にすると、アプリ一覧へ「Niri Android Monitor 設定」が追加されます。
開発ツリーから直接起動する場合は次を使います。

```bash
nix run .#gui
```

GUIではデーモンを停止せずに、以下を変更できます。

- niri output名
- 任意の幅・高さ・最大fps
- Auto / VA-API / x264 encoder
- ADB serial
- タッチ入力
- render nodeと明示的なcustom mode

設定は`~/.config/niri-android-monitor/settings.json`へ保存されます。NixOS moduleやコマンドライン
引数は初期値として扱われ、「Nix設定へ戻す」で保存した上書きを削除できます。

GUIとデーモンは`$XDG_RUNTIME_DIR/niri-android-monitor.sock`で通信します。socketはmode 0600で、
同じユーザーだけが操作できます。解像度・fps・encoderなどを適用すると、デーモンのsystemd
serviceは維持したまま現在の映像セッションだけを終了します。Androidアプリが自動再接続し、
通常は約1秒以内に新しい設定へ切り替わります。

上部にはADB・配信状態、実際に使われているencoder、実効fps、bitrate、Android受信から
MediaCodec outputまでの時間が表示されます。

## Development

Linux:

```bash
nix develop .#linux
cargo test
cargo clippy --all-targets -- -D warnings
```

Android:

```bash
nix develop .#android
cd android
gradle :app:installDebug
```

一時的にVKMSを作る場合:

```bash
sudo ./scripts/vkms-configfs.sh setup
niri msg output Virtual-1 off
```

手動起動:

```bash
nix run . -- \
  --adb-serial f2ccba87 \
  --output Virtual-1 \
  --width 1920 --height 1080 --fps 60
```

解像度とfpsは任意に変更できます。`--mode`を省略すると
`WIDTHxHEIGHT@FPS`がniri custom modeとして使われます。出力名、mode、encoderなど全引数は
`nix run . -- --help`で確認できます。ADB reverseは待機中に2秒間隔で再適用されるため、USBを
抜き差ししてもデーモンの再起動は不要です。

## Touch

現状は1本指をabsolute mouseとして扱います。

- down: ポインター移動 + 左ボタンdown
- move: absolute pointer移動
- up/cancel: 左ボタンup

タッチ座標は接続時のniri logical output位置へ変換されます。接続が切れた場合も左ボタンを必ず
releaseします。タッチを使わない場合は`--no-touch`またはNix option
`touch.enable = false`を指定します。

## 計測

Linux daemonは2秒ごとに実効fpsとbitrateをstderrへ出します。Androidは受信fps、bitrate、
受信完了からMediaCodec outputまでの時間をlogcatへ出します。

```bash
adb logcat -s 'NiriMonitor:*' '*:S'
```

この端末で確認した受信→デコード完了は、1920x1080@60の静止画更新時で概ね9〜19 msでした。
これはUSB到着後の区間だけで、niri描画開始からLCD表示までの真のend-to-end値ではありません。
end-to-endは高速撮影またはLED/photodiode方式で別に測る必要があります。

## Protocol

protocol v2は20-byte stream headerの後に、24-byte frame headerとAnnex-B H.264 access unitを
送ります。frame headerには実測monotonic PTS、sequence、key-frame flagが含まれます。同じ
TCP connectionの逆方向には16-byte touch messageを送ります。sequence gapと
receive-to-decode時間を使って、バッファ詰まりや欠落を切り分けられます。

## Verified on this machine

- Xiaomi Pad 6S Pro (`24018RPACG`, Android 16)、USB/ADB reverse
- Qualcomm hardware H.264 decoder + low-latency mode
- AMD VA-API + DMA-BUF
- 1920x1080@60、静止時約1 fps / `wf-recorder`約0.5% CPU
- 1本指tap・連続swipeのuinput転送
- アプリ終了およびSIGINT時の`Virtual-1`自動OFF
