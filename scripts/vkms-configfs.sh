#!/usr/bin/env bash
set -euo pipefail

instance="${VKMS_INSTANCE:-niri-android}"
base="/sys/kernel/config/vkms/${instance}"

setup() {
  modprobe vkms create_default_dev=0
  if [[ -e "$base/enabled" ]]; then
    echo "VKMS instance ${instance} already exists"
    return
  fi

  mkdir "$base"
  mkdir "$base/planes/plane0"
  mkdir "$base/crtcs/crtc0"
  mkdir "$base/encoders/encoder0"
  mkdir "$base/connectors/connector0"
  echo 1 > "$base/planes/plane0/type"
  ln -s "$base/crtcs/crtc0" "$base/planes/plane0/possible_crtcs/crtc0"
  ln -s "$base/crtcs/crtc0" "$base/encoders/encoder0/possible_crtcs/crtc0"
  ln -s "$base/encoders/encoder0" "$base/connectors/connector0/possible_encoders/encoder0"
  echo 1 > "$base/connectors/connector0/status"
  echo 1 > "$base/enabled"
  echo "created VKMS instance ${instance}"
}

teardown() {
  if [[ ! -e "$base/enabled" ]]; then
    echo "VKMS instance ${instance} does not exist"
    return
  fi

  echo 0 > "$base/enabled"
  unlink "$base/planes/plane0/possible_crtcs/crtc0"
  unlink "$base/encoders/encoder0/possible_crtcs/crtc0"
  unlink "$base/connectors/connector0/possible_encoders/encoder0"
  rmdir "$base/planes/plane0"
  rmdir "$base/crtcs/crtc0"
  rmdir "$base/encoders/encoder0"
  rmdir "$base/connectors/connector0"
  rmdir "$base"
  echo "removed VKMS instance ${instance}"
}

status() {
  if [[ -e "$base/enabled" ]]; then
    echo "instance=${instance} enabled=$(<"$base/enabled") connector_status=$(<"$base/connectors/connector0/status")"
  else
    echo "instance=${instance} absent"
  fi
}

case "${1:-status}" in
  setup) setup ;;
  teardown) teardown ;;
  status) status ;;
  *) echo "usage: $0 {setup|teardown|status}" >&2; exit 2 ;;
esac
