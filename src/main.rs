use serde::{Deserialize, Serialize};
use serde_json::Value;
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 4] = b"NAMD";
const PROTOCOL_VERSION: u16 = 2;
const STREAM_FLAG_TOUCH: u16 = 1;
const MAX_ACCESS_UNIT_SIZE: usize = 16 * 1024 * 1024;
const CONTROL_MESSAGE_SIZE: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    output: String,
    mode: String,
    width: u32,
    height: u32,
    fps: u32,
    port: u16,
    adb_serial: Option<String>,
    encoder: String,
    render_node: String,
    touch: bool,
    ydotool_socket: PathBuf,
}

impl Config {
    fn validate(&self) -> Result<(), String> {
        if self.width == 0 || self.height == 0 || self.fps == 0 {
            return Err("width, height, and fps must be non-zero".into());
        }
        if self.fps > 240 {
            return Err("fps must be 240 or lower".into());
        }
        if !matches!(self.encoder.as_str(), "auto" | "vaapi" | "x264") {
            return Err("encoder must be auto, vaapi, or x264".into());
        }
        if self.output.trim().is_empty() {
            return Err("output must not be empty".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
struct DaemonStatus {
    streaming: bool,
    adb_ready: bool,
    active_output: Option<String>,
    active_width: Option<u32>,
    active_height: Option<u32>,
    active_fps: Option<u32>,
    active_encoder: Option<String>,
    effective_fps: f64,
    bitrate_mbps: f64,
    android_decode_ms: Option<f64>,
}

struct SharedState {
    settings: RwLock<Config>,
    defaults: Config,
    status: Mutex<DaemonStatus>,
    revision: AtomicU64,
    config_path: PathBuf,
}

impl SharedState {
    fn settings(&self) -> Config {
        self.settings.read().unwrap().clone()
    }

    fn status(&self) -> DaemonStatus {
        self.status.lock().unwrap().clone()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output: "Virtual-1".into(),
            mode: String::new(),
            width: 1920,
            height: 1080,
            fps: 60,
            port: 57421,
            adb_serial: env::var("ANDROID_SERIAL").ok(),
            encoder: "auto".into(),
            render_node: "/dev/dri/renderD128".into(),
            touch: true,
            ydotool_socket: env::var_os("YDOTOOL_SOCKET")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/run/ydotoold/socket".into()),
        }
    }
}

fn main() -> io::Result<()> {
    let defaults = parse_args()?;
    let config_path = settings_path()?;
    let config = load_settings(&config_path, &defaults);
    config.validate().map_err(io::Error::other)?;
    let stopping = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stopping))?;
    flag::register(SIGTERM, Arc::clone(&stopping))?;
    let shared = Arc::new(SharedState {
        settings: RwLock::new(config.clone()),
        defaults: defaults.clone(),
        status: Mutex::new(DaemonStatus::default()),
        revision: AtomicU64::new(1),
        config_path,
    });
    let listener = TcpListener::bind(("127.0.0.1", config.port))?;
    listener.set_nonblocking(true)?;
    let control_socket = control_socket_path()?;
    let control_thread = spawn_control_server(
        control_socket.clone(),
        Arc::clone(&shared),
        Arc::clone(&stopping),
    )?;

    // VKMS stays registered, but it must not occupy a niri workspace until the
    // Android app actually connects.
    set_output_off(&config.output);
    if defaults.output != config.output {
        set_output_off(&defaults.output);
    }

    eprintln!(
        "waiting for Android on 127.0.0.1:{} (output {}, mode {})",
        config.port,
        config.output,
        mode_string(&config)
    );
    eprintln!("settings control socket: {}", control_socket.display());

    let mut next_adb_attempt = Instant::now();
    let mut adb_was_ready = false;
    while !stopping.load(Ordering::Relaxed) {
        if Instant::now() >= next_adb_attempt {
            let current = shared.settings();
            let ready = install_adb_reverse(&current);
            shared.status.lock().unwrap().adb_ready = ready;
            if ready && !adb_was_ready {
                eprintln!("ADB reverse ready");
            } else if !ready && adb_was_ready {
                eprintln!("Android USB connection lost; waiting for reconnect");
            }
            adb_was_ready = ready;
            next_adb_attempt = Instant::now() + Duration::from_secs(2);
        }

        match listener.accept() {
            Ok((stream, _)) => {
                eprintln!("Android connected");
                let current = shared.settings();
                let revision = shared.revision.load(Ordering::Acquire);
                if let Err(error) = serve(
                    stream,
                    &current,
                    Arc::clone(&stopping),
                    Arc::clone(&shared),
                    revision,
                ) {
                    eprintln!("stream ended: {error}");
                }
                reset_stream_status(&shared);
                eprintln!("output disabled; waiting for reconnect");
                next_adb_attempt = Instant::now();
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }

    set_output_off(&shared.settings().output);
    let _ = control_thread.join();
    let _ = std::fs::remove_file(&control_socket);
    eprintln!("stopped");
    Ok(())
}

fn parse_args() -> io::Result<Config> {
    let mut config = Config::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = |args: &mut std::iter::Skip<env::Args>, name: &str| {
            args.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing value for {name}"),
                )
            })
        };
        match arg.as_str() {
            "--output" => config.output = value(&mut args, "--output")?,
            "--mode" => config.mode = value(&mut args, "--mode")?,
            "--width" => config.width = parse(&value(&mut args, "--width")?, "width")?,
            "--height" => config.height = parse(&value(&mut args, "--height")?, "height")?,
            "--fps" => config.fps = parse(&value(&mut args, "--fps")?, "fps")?,
            "--port" => config.port = parse(&value(&mut args, "--port")?, "port")?,
            "--adb-serial" => config.adb_serial = Some(value(&mut args, "--adb-serial")?),
            "--encoder" => config.encoder = value(&mut args, "--encoder")?,
            "--render-node" => config.render_node = value(&mut args, "--render-node")?,
            "--ydotool-socket" => {
                config.ydotool_socket = value(&mut args, "--ydotool-socket")?.into()
            }
            "--no-touch" => config.touch = false,
            "--help" | "-h" => {
                println!(
                    r#"niri-android-monitor [--output Virtual-1] [--mode 1920x1080@60] \
                     [--width 1920] [--height 1080] [--fps 60] [--port 57421] \
                     [--adb-serial SERIAL] [--encoder auto|vaapi|x264] \
                     [--render-node /dev/dri/renderD128] \
                     [--ydotool-socket /run/ydotoold/socket] [--no-touch]"#
                );
                std::process::exit(0);
            }
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument: {unknown}"),
                ));
            }
        }
    }
    if config.mode.is_empty() {
        config.mode = format!("{}x{}@{}", config.width, config.height, config.fps);
    }
    config
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(config)
}

fn mode_string(config: &Config) -> String {
    if config.mode.trim().is_empty() {
        format!("{}x{}@{}", config.width, config.height, config.fps)
    } else {
        config.mode.clone()
    }
}

fn settings_path() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path)
            .join("niri-android-monitor")
            .join("settings.json"));
    }
    let home = env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("niri-android-monitor")
        .join("settings.json"))
}

fn control_socket_path() -> io::Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
    Ok(PathBuf::from(runtime).join("niri-android-monitor.sock"))
}

fn load_settings(path: &Path, defaults: &Config) -> Config {
    match std::fs::read(path) {
        Ok(data) => match serde_json::from_slice::<Config>(&data) {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("ignoring invalid settings file {}: {error}", path.display());
                defaults.clone()
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => defaults.clone(),
        Err(error) => {
            eprintln!("could not read settings file {}: {error}", path.display());
            defaults.clone()
        }
    }
}

fn save_settings(path: &Path, settings: &Config) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("settings path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
    std::fs::write(&temporary, data)?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(temporary, path)
}

fn spawn_control_server(
    path: PathBuf,
    shared: Arc<SharedState>,
    stopping: Arc<AtomicBool>,
) -> io::Result<thread::JoinHandle<()>> {
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another daemon owns {}", path.display()),
            ));
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => handle_control_client(stream, &shared),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    eprintln!("control socket accept failed: {error}");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }))
}

fn handle_control_client(mut stream: UnixStream, shared: &SharedState) {
    let result = (|| -> Result<Value, String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| error.to_string())?;
        let mut request_line = String::new();
        BufReader::new(stream.try_clone().map_err(|error| error.to_string())?)
            .take(1024 * 1024)
            .read_line(&mut request_line)
            .map_err(|error| error.to_string())?;
        let request: Value =
            serde_json::from_str(&request_line).map_err(|error| error.to_string())?;
        let command = request
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing command".to_string())?;

        match command {
            "get" => Ok(serde_json::json!({
                "settings": shared.settings(),
                "status": shared.status(),
                "revision": shared.revision.load(Ordering::Acquire),
            })),
            "status" => Ok(serde_json::json!({
                "status": shared.status(),
                "revision": shared.revision.load(Ordering::Acquire),
            })),
            "set" => {
                let mut settings: Config = serde_json::from_value(
                    request
                        .get("settings")
                        .cloned()
                        .ok_or_else(|| "missing settings".to_string())?,
                )
                .map_err(|error| error.to_string())?;
                settings.validate()?;
                let old = shared.settings();
                if settings.port != old.port {
                    return Err("the TCP port requires a daemon restart".into());
                }
                if settings.mode == format!("{}x{}@{}", old.width, old.height, old.fps)
                    && (settings.width, settings.height, settings.fps)
                        != (old.width, old.height, old.fps)
                {
                    settings.mode.clear();
                }
                save_settings(&shared.config_path, &settings).map_err(|error| error.to_string())?;
                *shared.settings.write().unwrap() = settings.clone();
                shared.revision.fetch_add(1, Ordering::AcqRel);
                if old.output != settings.output {
                    set_output_off(&old.output);
                }
                Ok(serde_json::json!({ "settings": settings, "restarting_stream": true }))
            }
            "reset" => {
                let old = shared.settings();
                if shared.defaults.port != old.port {
                    return Err("resetting the TCP port requires a daemon restart".into());
                }
                if let Err(error) = std::fs::remove_file(&shared.config_path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    return Err(error.to_string());
                }
                let settings = shared.defaults.clone();
                *shared.settings.write().unwrap() = settings.clone();
                shared.revision.fetch_add(1, Ordering::AcqRel);
                if old.output != settings.output {
                    set_output_off(&old.output);
                }
                Ok(serde_json::json!({ "settings": settings, "restarting_stream": true }))
            }
            _ => Err(format!("unknown command: {command}")),
        }
    })();

    let response = match result {
        Ok(payload) => serde_json::json!({ "ok": true, "result": payload }),
        Err(error) => serde_json::json!({ "ok": false, "error": error }),
    };
    if let Ok(mut encoded) = serde_json::to_vec(&response) {
        encoded.push(b'\n');
        let _ = stream.write_all(&encoded);
    }
}

fn reset_stream_status(shared: &SharedState) {
    let mut status = shared.status.lock().unwrap();
    status.streaming = false;
    status.active_output = None;
    status.active_width = None;
    status.active_height = None;
    status.active_fps = None;
    status.active_encoder = None;
    status.effective_fps = 0.0;
    status.bitrate_mbps = 0.0;
    status.android_decode_ms = None;
}

fn parse<T: std::str::FromStr>(value: &str, name: &str) -> io::Result<T> {
    value.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {name}: {value}"),
        )
    })
}

fn install_adb_reverse(config: &Config) -> bool {
    let mut command = Command::new("adb");
    if let Some(serial) = &config.adb_serial {
        command.args(["-s", serial]);
    }
    let mapping = format!("tcp:{}", config.port);
    command
        .args(["reverse", &mapping, &mapping])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn niri_output(output: &str, action: &[&str]) -> io::Result<()> {
    let status = Command::new("niri")
        .args(["msg", "output", output])
        .args(action)
        .stdout(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "niri output action failed: {}",
            action.join(" ")
        )))
    }
}

fn set_output_off(output: &str) {
    if let Err(error) = niri_output(output, &["off"]) {
        eprintln!("could not disable {output}: {error}");
    }
}

struct ActiveOutput<'a>(&'a str);

impl Drop for ActiveOutput<'_> {
    fn drop(&mut self) {
        set_output_off(self.0);
    }
}

fn serve(
    mut stream: TcpStream,
    config: &Config,
    stopping: Arc<AtomicBool>,
    shared: Arc<SharedState>,
    session_revision: u64,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;

    // Install the cleanup guard before the first output mutation: custom-mode
    // itself may activate a previously disabled connector on some niri builds.
    let _output_guard = ActiveOutput(&config.output);
    niri_output(&config.output, &["custom-mode", &mode_string(config)])?;
    niri_output(&config.output, &["scale", "1"])?;
    niri_output(&config.output, &["on"])?;
    thread::sleep(Duration::from_millis(250));

    let geometry = query_output_geometry(config);
    write_header(&mut stream, config)?;
    let (mut recorder, active_encoder) = spawn_recorder(config)?;
    let stdout = recorder
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("wf-recorder stdout was not piped"))?;
    {
        let mut status = shared.status.lock().unwrap();
        status.streaming = true;
        status.active_output = Some(config.output.clone());
        status.active_width = Some(config.width);
        status.active_height = Some(config.height);
        status.active_fps = Some(config.fps);
        status.active_encoder = Some(active_encoder);
    }

    let control_stream = stream.try_clone()?;
    let touch_socket = config.touch.then(|| config.ydotool_socket.clone());
    let control_shared = Arc::clone(&shared);
    let control = thread::spawn(move || {
        control_loop(
            control_stream,
            &mut recorder,
            geometry,
            touch_socket,
            stopping,
            control_shared,
            session_revision,
        )
    });

    let result = forward_access_units(BufReader::new(stdout), &mut stream, &shared);
    let _ = stream.shutdown(Shutdown::Both);
    let _ = control.join();
    result
}

fn write_header(stream: &mut TcpStream, config: &Config) -> io::Result<()> {
    let flags = if config.touch { STREAM_FLAG_TOUCH } else { 0 };
    stream.write_all(MAGIC)?;
    stream.write_all(&PROTOCOL_VERSION.to_be_bytes())?;
    stream.write_all(&flags.to_be_bytes())?;
    stream.write_all(&config.width.to_be_bytes())?;
    stream.write_all(&config.height.to_be_bytes())?;
    stream.write_all(&(config.fps * 1000).to_be_bytes())?;
    stream.flush()
}

fn spawn_recorder(config: &Config) -> io::Result<(Child, String)> {
    if config.encoder == "auto" && Path::new(&config.render_node).exists() {
        eprintln!("trying vaapi encoder on {}", config.render_node);
        let mut child = spawn_encoder(config, "vaapi")?;
        thread::sleep(Duration::from_millis(150));
        if let Some(status) = child.try_wait()? {
            eprintln!("vaapi exited early ({status}); falling back to x264");
            return spawn_encoder(config, "x264").map(|child| (child, "x264".into()));
        }
        return Ok((child, "vaapi".into()));
    }

    let encoder = if config.encoder == "auto" {
        "x264"
    } else {
        config.encoder.as_str()
    };
    eprintln!("using {encoder} encoder");
    spawn_encoder(config, encoder).map(|child| (child, encoder.into()))
}

fn spawn_encoder(config: &Config, encoder: &str) -> io::Result<Child> {
    let mut command = Command::new("wf-recorder");
    command.args([
        "-o",
        &config.output,
        // -B tells wf-recorder the maximum capture rate while retaining
        // damage-driven VFR. Unlike -r, it does not duplicate static frames.
        "-B",
        &config.fps.to_string(),
    ]);

    if encoder == "vaapi" {
        command.args([
            "-c",
            "h264_vaapi",
            "-d",
            &config.render_node,
            "-b",
            "0",
            "-p",
            "aud=1",
            "-p",
            &format!("g={}", config.fps),
            "-p",
            "rc_mode=CQP",
            "-p",
            "qp=20",
        ]);
    } else {
        let x264_params = format!(
            "aud=1:repeat-headers=1:keyint={}:min-keyint={}:scenecut=0",
            config.fps, config.fps
        );
        command.args([
            "-c",
            "libx264",
            "-b",
            "0",
            "-p",
            "preset=ultrafast",
            "-p",
            "tune=zerolatency",
            "-p",
            &format!("x264-params={x264_params}"),
        ]);
    }

    command
        .args(["-m", "h264", "-y", "-f", "/dev/stdout"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
}

#[derive(Clone, Copy, Debug)]
struct OutputGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn query_output_geometry(config: &Config) -> OutputGeometry {
    let fallback = OutputGeometry {
        x: 0,
        y: 0,
        width: config.width,
        height: config.height,
    };
    let Ok(output) = Command::new("niri").args(["msg", "-j", "outputs"]).output() else {
        return fallback;
    };
    let Ok(json) = serde_json::from_slice::<Value>(&output.stdout) else {
        return fallback;
    };
    let Some(logical) = json
        .get(&config.output)
        .and_then(|value| value.get("logical"))
    else {
        return fallback;
    };
    let number = |name: &str| logical.get(name).and_then(Value::as_i64);
    OutputGeometry {
        x: number("x").and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
        y: number("y").and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
        width: number("width")
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(config.width),
        height: number("height")
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(config.height),
    }
}

fn control_loop(
    mut stream: TcpStream,
    recorder: &mut Child,
    geometry: OutputGeometry,
    socket_path: Option<PathBuf>,
    stopping: Arc<AtomicBool>,
    shared: Arc<SharedState>,
    session_revision: u64,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut touch = socket_path.and_then(|path| match YdotoolClient::connect(&path) {
        Ok(client) => {
            eprintln!("touch forwarding enabled via {}", path.display());
            Some(client)
        }
        Err(error) => {
            eprintln!("touch forwarding unavailable ({}): {error}", path.display());
            None
        }
    });
    let mut message = [0u8; CONTROL_MESSAGE_SIZE];
    let mut filled = 0;

    while !stopping.load(Ordering::Relaxed)
        && shared.revision.load(Ordering::Acquire) == session_revision
    {
        match stream.read(&mut message[filled..]) {
            Ok(0) => break,
            Ok(count) => {
                filled += count;
                if filled == message.len() {
                    match message[0] {
                        1 => {
                            if let Some(client) = touch.as_mut()
                                && let Err(error) = client.handle(&message, geometry)
                            {
                                eprintln!("touch event failed: {error}");
                            }
                        }
                        2 => {
                            let decode_ms = f32::from_bits(u32::from_be_bytes(
                                message[4..8].try_into().unwrap(),
                            ));
                            if decode_ms.is_finite() {
                                shared.status.lock().unwrap().android_decode_ms =
                                    Some(f64::from(decode_ms));
                            }
                        }
                        _ => {}
                    }
                    filled = 0;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => break,
        }
    }

    if let Some(client) = touch.as_mut() {
        let _ = client.release_button();
    }
    let _ = recorder.kill();
    let _ = recorder.wait();
    let _ = stream.shutdown(Shutdown::Both);
}

struct YdotoolClient {
    socket: UnixDatagram,
    button_down: bool,
}

impl YdotoolClient {
    fn connect(path: &Path) -> io::Result<Self> {
        let socket = UnixDatagram::unbound()?;
        socket.connect(path)?;
        Ok(Self {
            socket,
            button_down: false,
        })
    }

    fn handle(
        &mut self,
        message: &[u8; CONTROL_MESSAGE_SIZE],
        geometry: OutputGeometry,
    ) -> io::Result<()> {
        if message[0] != 1 {
            return Ok(());
        }
        let action = message[1];
        let x =
            f32::from_bits(u32::from_be_bytes(message[4..8].try_into().unwrap())).clamp(0.0, 1.0);
        let y =
            f32::from_bits(u32::from_be_bytes(message[8..12].try_into().unwrap())).clamp(0.0, 1.0);
        if !x.is_finite() || !y.is_finite() {
            return Ok(());
        }
        let target_x = geometry
            .x
            .saturating_add((x * geometry.width.saturating_sub(1) as f32).round() as i32);
        let target_y = geometry
            .y
            .saturating_add((y * geometry.height.saturating_sub(1) as f32).round() as i32);
        self.move_absolute(target_x, target_y)?;

        match action {
            0 if !self.button_down => {
                self.send_events(&[(1, 0x110, 1), (0, 0, 0)])?;
                self.button_down = true;
            }
            2 | 3 if self.button_down => self.release_button()?,
            _ => {}
        }
        Ok(())
    }

    fn move_absolute(&self, x: i32, y: i32) -> io::Result<()> {
        self.send_events(&[
            (2, 0, i32::MIN),
            (2, 1, i32::MIN),
            (2, 0, x),
            (2, 1, y),
            (0, 0, 0),
        ])
    }

    fn release_button(&mut self) -> io::Result<()> {
        if self.button_down {
            self.send_events(&[(1, 0x110, 0), (0, 0, 0)])?;
            self.button_down = false;
        }
        Ok(())
    }

    fn send_events(&self, events: &[(u16, u16, i32)]) -> io::Result<()> {
        for (event_type, code, value) in events {
            let mut packet = Vec::with_capacity(24);
            packet.extend_from_slice(&[0; 16]); // timeval; ydotoold ignores it.
            packet.extend_from_slice(&event_type.to_ne_bytes());
            packet.extend_from_slice(&code.to_ne_bytes());
            packet.extend_from_slice(&value.to_ne_bytes());
            // ydotoold receives exactly one input_event per datagram.
            let sent = self.socket.send(&packet)?;
            if sent != packet.len() {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short ydotool send",
                ));
            }
        }
        Ok(())
    }
}

fn forward_access_units<R: Read>(
    mut input: R,
    output: &mut TcpStream,
    shared: &SharedState,
) -> io::Result<()> {
    let started = Instant::now();
    let mut parser = AnnexBParser::default();
    let mut chunk = [0u8; 64 * 1024];
    let mut access_unit = Vec::new();
    let mut sequence = 0u64;
    let mut last_pts = 0u64;
    let mut stats = StreamStats::new(shared);

    loop {
        let count = input.read(&mut chunk)?;
        if count == 0 {
            if let Some(nal) = parser.finish() {
                consume_nal(
                    nal,
                    &mut access_unit,
                    output,
                    started,
                    &mut sequence,
                    &mut last_pts,
                    &mut stats,
                )?;
            }
            if !access_unit.is_empty() {
                send_access_unit(
                    output,
                    &access_unit,
                    started,
                    sequence,
                    &mut last_pts,
                    &mut stats,
                )?;
            }
            return Ok(());
        }
        parser.push(&chunk[..count]);
        while let Some(nal) = parser.next_nal() {
            consume_nal(
                nal,
                &mut access_unit,
                output,
                started,
                &mut sequence,
                &mut last_pts,
                &mut stats,
            )?;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn consume_nal(
    nal: Vec<u8>,
    access_unit: &mut Vec<u8>,
    output: &mut TcpStream,
    started: Instant,
    sequence: &mut u64,
    last_pts: &mut u64,
    stats: &mut StreamStats,
) -> io::Result<()> {
    let nal_type = nal_type(&nal).unwrap_or(0);
    if nal_type == 9 && !access_unit.is_empty() {
        send_access_unit(output, access_unit, started, *sequence, last_pts, stats)?;
        *sequence += 1;
        access_unit.clear();
    }
    if access_unit.len() + nal.len() > MAX_ACCESS_UNIT_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "H.264 access unit exceeded 16 MiB",
        ));
    }
    access_unit.extend_from_slice(&nal);
    Ok(())
}

fn send_access_unit(
    output: &mut TcpStream,
    access_unit: &[u8],
    started: Instant,
    sequence: u64,
    last_pts: &mut u64,
    stats: &mut StreamStats,
) -> io::Result<()> {
    let length = u32::try_from(access_unit.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "access unit too large"))?;
    let measured_pts = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let pts_us = measured_pts.max(last_pts.saturating_add(1));
    *last_pts = pts_us;
    let flags = u32::from(contains_nal_type(access_unit, 5));
    output.write_all(&length.to_be_bytes())?;
    output.write_all(&pts_us.to_be_bytes())?;
    output.write_all(&sequence.to_be_bytes())?;
    output.write_all(&flags.to_be_bytes())?;
    output.write_all(access_unit)?;
    stats.record(access_unit.len());
    Ok(())
}

struct StreamStats<'a> {
    window_started: Instant,
    frames: u64,
    bytes: u64,
    shared: &'a SharedState,
}

impl<'a> StreamStats<'a> {
    fn new(shared: &'a SharedState) -> Self {
        Self {
            window_started: Instant::now(),
            frames: 0,
            bytes: 0,
            shared,
        }
    }

    fn record(&mut self, bytes: usize) {
        self.frames += 1;
        self.bytes += bytes as u64;
        let elapsed = self.window_started.elapsed();
        if elapsed >= Duration::from_secs(2) {
            let seconds = elapsed.as_secs_f64();
            let effective_fps = self.frames as f64 / seconds;
            let bitrate_mbps = self.bytes as f64 * 8.0 / seconds / 1_000_000.0;
            eprintln!(
                "stream: {:.1} fps, {:.2} Mbit/s (damage-driven)",
                effective_fps, bitrate_mbps
            );
            let mut status = self.shared.status.lock().unwrap();
            status.effective_fps = effective_fps;
            status.bitrate_mbps = bitrate_mbps;
            self.window_started = Instant::now();
            self.frames = 0;
            self.bytes = 0;
        }
    }
}

fn contains_nal_type(data: &[u8], wanted: u8) -> bool {
    let mut offset = 0;
    while let Some((start, prefix)) = find_start_code(data, offset) {
        if data.get(start + prefix).map(|byte| byte & 0x1f) == Some(wanted) {
            return true;
        }
        offset = start + prefix;
    }
    false
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    let (start, prefix) = find_start_code(nal, 0)?;
    nal.get(start + prefix).map(|byte| byte & 0x1f)
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if index + 4 <= data.len() && data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

#[derive(Default)]
struct AnnexBParser {
    buffer: Vec<u8>,
}

impl AnnexBParser {
    fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    fn next_nal(&mut self) -> Option<Vec<u8>> {
        let (first, _) = find_start_code(&self.buffer, 0)?;
        if first > 0 {
            self.buffer.drain(..first);
        }
        let (_, prefix) = find_start_code(&self.buffer, 0)?;
        let (next, _) = find_start_code(&self.buffer, prefix)?;
        Some(self.buffer.drain(..next).collect())
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        if find_start_code(&self.buffer, 0).is_some() {
            Some(std::mem::take(&mut self.buffer))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_across_chunks() {
        let mut parser = AnnexBParser::default();
        parser.push(&[0, 0]);
        assert!(parser.next_nal().is_none());
        parser.push(&[0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1]);
        let first = parser.next_nal().unwrap();
        assert_eq!(nal_type(&first), Some(9));
        parser.push(&[2, 0, 0, 1, 0x68, 3]);
        let second = parser.next_nal().unwrap();
        assert_eq!(nal_type(&second), Some(7));
        assert_eq!(nal_type(&parser.finish().unwrap()), Some(8));
    }

    #[test]
    fn control_message_coordinates_are_big_endian_floats() {
        let mut message = [0u8; CONTROL_MESSAGE_SIZE];
        message[0] = 1;
        message[4..8].copy_from_slice(&0.25f32.to_bits().to_be_bytes());
        message[8..12].copy_from_slice(&0.75f32.to_bits().to_be_bytes());
        assert_eq!(
            f32::from_bits(u32::from_be_bytes(message[4..8].try_into().unwrap())),
            0.25
        );
        assert_eq!(
            f32::from_bits(u32::from_be_bytes(message[8..12].try_into().unwrap())),
            0.75
        );
    }
}
