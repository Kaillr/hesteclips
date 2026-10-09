# Fedora with KDE's compositor (kwin, virtual backend) and plasma-workspace,
# which brings the app menu KWin's permission check needs and the tray host
# (kded6). KWin can't stream the screen here (no GPU), so the KDE tests check
# permissions, monitors, the window list and the tray, not the picture.
FROM fedora:42
RUN dnf install -y --setopt=install_weak_deps=False \
    kwin-wayland plasma-workspace kf6-kservice pipewire wireplumber dbus-daemon \
    gcc clang-devel pipewire-devel dbus-devel alsa-lib-devel \
    ffmpeg-free procps-ng mesa-dri-drivers foot \
    && dnf clean all && (setcap -r /usr/bin/kwin_wayland || true)
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH=/root/.cargo/bin:$PATH
