# Ubuntu 24.04 with what the tests need: the build tools, an X server with a
# dummy video driver (several virtual monitors; Xvfb can't make RandR
# monitors), Sway (headless), a window manager to focus and close windows,
# and PipeWire for sound.
FROM ubuntu:24.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y \
    build-essential curl pkg-config libclang-dev libpipewire-0.3-dev libdbus-1-dev libasound2-dev patchelf \
    xserver-xorg-core xserver-xorg-video-dummy x11-xserver-utils x11-utils x11-apps xterm openbox wmctrl \
    libxkbcommon-x11-0 libgl1 libegl1 mesa-vulkan-drivers \
    sway foot \
    pipewire pipewire-pulse wireplumber pipewire-bin pulseaudio-utils dbus \
    ffmpeg imagemagick python3 \
    && rm -rf /var/lib/apt/lists/*
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
COPY xorg.conf /xorg.conf
