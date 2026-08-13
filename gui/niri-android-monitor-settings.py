#!/usr/bin/env python3
import json
import os
import socket
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk


APP_ID = "dev.niri.androidmonitor.Settings"


def control_socket_path():
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if not runtime:
        raise RuntimeError("XDG_RUNTIME_DIR is not set")
    return Path(runtime) / "niri-android-monitor.sock"


def request(payload):
    encoded = (json.dumps(payload, separators=(",", ":")) + "\n").encode()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(1.5)
        connection.connect(str(control_socket_path()))
        connection.sendall(encoded)
        response = b""
        while not response.endswith(b"\n"):
            chunk = connection.recv(65536)
            if not chunk:
                break
            response += chunk
            if len(response) > 1024 * 1024:
                raise RuntimeError("The daemon response is too large")
    if not response:
        raise RuntimeError("The daemon did not respond")
    decoded = json.loads(response)
    if not decoded.get("ok"):
        raise RuntimeError(decoded.get("error", "Settings API error"))
    return decoded["result"]


class SettingsWindow(Gtk.ApplicationWindow):
    def __init__(self, application):
        super().__init__(application=application, title="Niri Android Monitor Settings")
        self.set_default_size(620, 610)
        self.settings = None

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        outer.set_margin_top(24)
        outer.set_margin_bottom(24)
        outer.set_margin_start(28)
        outer.set_margin_end(28)
        self.set_child(outer)

        title = Gtk.Label(label="Niri Android Monitor")
        title.add_css_class("title-1")
        title.set_xalign(0)
        outer.append(title)

        self.connection_label = Gtk.Label(label="Connecting to the daemon…")
        self.connection_label.set_xalign(0)
        self.connection_label.set_wrap(True)
        outer.append(self.connection_label)

        self.metrics_label = Gtk.Label(label="")
        self.metrics_label.set_xalign(0)
        self.metrics_label.set_wrap(True)
        self.metrics_label.add_css_class("dim-label")
        outer.append(self.metrics_label)

        separator = Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL)
        outer.append(separator)

        grid = Gtk.Grid(column_spacing=16, row_spacing=12)
        grid.set_hexpand(True)
        outer.append(grid)

        self.output_entry = Gtk.Entry()
        self.output_entry.set_hexpand(True)
        self._row(grid, 0, "niri output", self.output_entry)

        resolution = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        self.width_spin = Gtk.SpinButton.new_with_range(320, 8192, 1)
        self.height_spin = Gtk.SpinButton.new_with_range(240, 8192, 1)
        resolution.append(self.width_spin)
        resolution.append(Gtk.Label(label="×"))
        resolution.append(self.height_spin)
        self._row(grid, 1, "Resolution", resolution)

        self.fps_spin = Gtk.SpinButton.new_with_range(1, 240, 1)
        self._row(grid, 2, "Maximum FPS", self.fps_spin)

        self.adb_entry = Gtk.Entry()
        self.adb_entry.set_placeholder_text("Leave empty to use the only connected device")
        self.adb_entry.set_hexpand(True)
        self._row(grid, 3, "ADB serial", self.adb_entry)

        self.touch_switch = Gtk.Switch()
        self.touch_switch.set_halign(Gtk.Align.START)
        self._row(grid, 4, "Touch input", self.touch_switch)

        advanced = Gtk.Expander(label="Advanced settings")
        advanced_grid = Gtk.Grid(column_spacing=16, row_spacing=12)
        advanced_grid.set_margin_top(12)
        advanced.set_child(advanced_grid)
        outer.append(advanced)

        self.mode_entry = Gtk.Entry()
        self.mode_entry.set_placeholder_text("Leave empty for WIDTHxHEIGHT@FPS")
        self.mode_entry.set_hexpand(True)
        self._row(advanced_grid, 0, "custom mode", self.mode_entry)

        buttons = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        buttons.set_halign(Gtk.Align.END)
        outer.append(buttons)

        reset = Gtk.Button(label="Restore Nix defaults")
        reset.connect("clicked", self.on_reset)
        buttons.append(reset)

        reload_button = Gtk.Button(label="Reload")
        reload_button.connect("clicked", lambda _button: self.load_all())
        buttons.append(reload_button)

        apply_button = Gtk.Button(label="Apply")
        apply_button.add_css_class("suggested-action")
        apply_button.connect("clicked", self.on_apply)
        buttons.append(apply_button)

        self.notice_label = Gtk.Label(label="")
        self.notice_label.set_xalign(0)
        self.notice_label.set_wrap(True)
        outer.append(self.notice_label)

        GLib.idle_add(self.load_all)
        GLib.timeout_add_seconds(1, self.refresh_status)

    @staticmethod
    def _row(grid, row, label, widget):
        caption = Gtk.Label(label=label)
        caption.set_xalign(0)
        caption.set_halign(Gtk.Align.START)
        grid.attach(caption, 0, row, 1, 1)
        grid.attach(widget, 1, row, 1, 1)

    def load_all(self):
        try:
            result = request({"command": "get"})
            self.settings = result["settings"]
            self.populate(self.settings)
            self.show_status(result["status"])
            self.notice_label.set_text("")
        except Exception as error:
            self.show_error(error)
        return GLib.SOURCE_REMOVE

    def populate(self, settings):
        self.output_entry.set_text(settings["output"])
        self.width_spin.set_value(settings["width"])
        self.height_spin.set_value(settings["height"])
        self.fps_spin.set_value(settings["fps"])
        self.adb_entry.set_text(settings.get("adb_serial") or "")
        self.touch_switch.set_active(bool(settings.get("touch", True)))
        mode = settings.get("mode", "")
        derived = f'{settings["width"]}x{settings["height"]}@{settings["fps"]}'
        self.mode_entry.set_text("" if mode == derived else mode)

    def collect(self):
        if self.settings is None:
            raise RuntimeError("Settings have not been loaded yet")
        updated = dict(self.settings)
        updated.update(
            output=self.output_entry.get_text().strip(),
            width=self.width_spin.get_value_as_int(),
            height=self.height_spin.get_value_as_int(),
            fps=self.fps_spin.get_value_as_int(),
            adb_serial=self.adb_entry.get_text().strip() or None,
            touch=self.touch_switch.get_active(),
            mode=self.mode_entry.get_text().strip(),
        )
        return updated

    def on_apply(self, _button):
        try:
            result = request({"command": "set", "settings": self.collect()})
            self.settings = result["settings"]
            self.populate(self.settings)
            self.notice_label.set_text(
                "Settings applied. The video session is reconnecting if it was active."
            )
        except Exception as error:
            self.show_error(error)

    def on_reset(self, _button):
        try:
            result = request({"command": "reset"})
            self.settings = result["settings"]
            self.populate(self.settings)
            self.notice_label.set_text(
                "Saved overrides removed. Restored the Nix/service defaults."
            )
        except Exception as error:
            self.show_error(error)

    def refresh_status(self):
        try:
            result = request({"command": "status"})
            self.show_status(result["status"])
        except Exception as error:
            self.show_error(error, status_only=True)
        return GLib.SOURCE_CONTINUE

    def show_status(self, status):
        adb = "USB connected" if status.get("adb_ready") else "Waiting for Android"
        if status.get("streaming"):
            mode = f'{status.get("active_width")}×{status.get("active_height")} @ {status.get("active_fps")} fps'
            self.connection_label.set_text(
                f'Streaming — {status.get("active_output")} / {mode} / {status.get("active_encoder")}'
            )
        else:
            self.connection_label.set_text(f"Daemon running — {adb}; waiting for the app")
        decode = status.get("android_decode_ms")
        decode_text = "not measured" if decode is None else f"{decode:.2f} ms"
        self.metrics_label.set_text(
            f'Effective {status.get("effective_fps", 0):.1f} fps  ·  '
            f'{status.get("bitrate_mbps", 0):.2f} Mbit/s  ·  '
            f'Android receive-to-decode {decode_text}'
        )

    def show_error(self, error, status_only=False):
        self.connection_label.set_text(f"Cannot connect to the daemon: {error}")
        if not status_only:
            self.notice_label.set_text(str(error))


class SettingsApplication(Gtk.Application):
    def __init__(self):
        super().__init__(application_id=APP_ID)

    def do_activate(self):
        window = self.props.active_window
        if window is None:
            window = SettingsWindow(self)
        window.present()


if __name__ == "__main__":
    raise SystemExit(SettingsApplication().run(None))
