# niri Android monitor

USB-connected Android deviceを、niriの低遅延サブモニターとして使う実験実装です。

```text
VKMS / niri output → wlr-screencopy SHM → low-latency x264 H.264
                 → ADB reverse → MediaCodec → SurfaceView
Android touch → 同じTCP接続 → niri virtual pointer
```

niri本体への変更と物理ダミープラグは不要です。VKMS connectorはカーネルに登録したままにし、
Androidアプリが接続している間だけniri outputを有効にします。アプリ終了、USB切断、デーモンの
SIGINT/SIGTERM、エンコーダ異常終了のいずれでもoutputをOFFへ戻します。

## Android releases

Pushing a version tag such as `v0.1.0` creates a GitHub Release and attaches an optimized,
release-signed Android APK built by GitHub Actions. R8 code shrinking, resource shrinking, and an
English-only resource configuration keep the direct-install artifact small. The app has no native
libraries, so ABI-specific APKs would not materially reduce its size; one optimized universal APK
works across supported Android devices.

Before the first release, configure these repository Actions secrets. Use one persistent keystore
for every release so Android accepts updates over an already-installed APK.

- `ANDROID_KEYSTORE_BASE64`: base64-encoded release keystore
- `ANDROID_KEYSTORE_PASSWORD`
- `ANDROID_KEY_ALIAS`
- `ANDROID_KEY_PASSWORD`

For example, generate the keystore once and encode it without line wrapping:

```bash
keytool -genkeypair -keystore niri-monitor-release.keystore -alias niri-monitor \
  -keyalg RSA -keysize 4096 -validity 10000
base64 -w 0 niri-monitor-release.keystore
```

```bash
git tag v0.1.0
git push origin v0.1.0
```

## 省電力と遅延の方針

`--fps`は最大フレームレートです。キャプチャはdamage駆動のVFRで、静止画を指定fpsまで複製
しません。したがって、動きが止まるとエンコーダとUSB転送もほぼ停止し、次のdamageは直ちに
処理されます。LCDのリフレッシュレートを明示的に切り替えないため、再び動いた時のモード切替
待ちも入りません。

エンコードは低遅延x264へ固定し、wf-recorderのDMA-BUFキャプチャは常に無効化します。映像履歴や
GPU共有バッファを蓄積せず、上限付きの共有メモリバッファを循環再利用します。静止時はdamageが
発生した時だけ処理し、動きがある時だけ必要なfpsまで上がります。

## NixOSで常用する

flakeのNixOS moduleはVKMSとユーザーサービスをまとめて設定します。

```nix
{
  inputs.niri-android-monitor.url = "github:m-tky/niri-monitor";

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
    users = [ "user" ];
  };
}
```

複数のユーザーが同じマシンを使う場合は、次のように指定できます。

```nix
services.niri-android-monitor.users = [ "alice" "bob" ];
```

NixOS側ではサービスの有効化とデスクトップユーザーだけを指定します。解像度、fps、ADB serial、
output名、タッチ入力などの実際の設定はGUIから行います。GUIで保存した
`~/.config/niri-android-monitor/settings.json`がNix moduleの初期値より優先されるため、設定変更の
たびにNixOSをrebuildする必要はありません。

ユーザーサービスはniri IPC socketを待ってから起動し、最初に`Virtual-1`をOFFにします。
Androidアプリが閉じている
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
- 自動配置、または負数を含む任意の論理X/Y座標
- ADB serial
- タッチ入力
- 明示的なcustom mode

設定は`~/.config/niri-android-monitor/settings.json`へ保存されます。NixOS moduleやコマンドライン
引数は初期値として扱われ、「Restore Nix defaults」で保存した上書きを削除できます。

GUIとデーモンは`$XDG_RUNTIME_DIR/niri-android-monitor.sock`で通信します。socketはmode 0600で、
同じユーザーだけが操作できます。解像度・fpsなどを適用すると、デーモンのsystemd
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
  --width 1920 --height 1080 --fps 60 \
  --position-x 3440 --position-y 0
```

解像度とfpsは任意に変更できます。`--mode`を省略すると
`WIDTHxHEIGHT@FPS`がniri custom modeとして使われます。出力名、modeなど全引数は
`nix run . -- --help`で確認できます。ADB reverseは待機中に2秒間隔で再適用されるため、USBを
抜き差ししてもデーモンの再起動は不要です。

## Touch

1本指をabsolute mouseとして扱います。niriのWayland virtual pointerへdesktop全体の
絶対座標を直接送るため、マウス加速の影響を受けません。

- down: ポインター移動 + 左ボタンdown
- move: absolute pointer移動
- up/cancel: 左ボタンup

タッチ座標はAndroidの映像Surfaceから、接続時のniri logical output位置へ変換されます。接続が
切れた場合も左ボタンを必ずreleaseします。タッチを使わない場合は`--no-touch`またはNix option
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
- DMA-BUFを使わない共有メモリキャプチャ + low-latency x264
- 1920x1080@60、damage駆動VFR
- 1本指tap・連続swipeのniri virtual pointer転送
- アプリ終了およびSIGINT時の`Virtual-1`自動OFF
