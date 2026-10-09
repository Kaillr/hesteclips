# Fedora with GNOME's compositor (mutter, headless with virtual monitors) and
# XWayland, for the GNOME screen capture and game detection tests.
FROM fedora:42
RUN dnf install -y --setopt=install_weak_deps=False \
    mutter xorg-x11-server-Xwayland pipewire wireplumber dbus-daemon \
    gcc clang-devel pipewire-devel dbus-devel alsa-lib-devel \
    ffmpeg-free procps-ng mesa-dri-drivers xterm foot \
    && dnf clean all
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
