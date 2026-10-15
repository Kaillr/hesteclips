
cd /src
# Two "releases": 1.0.0 installed, 1.1.0 on the fake server. Built without ffmpeg (not what's tested).
build() {
  v=$1; out=/tmp/rel-$v/HasteClip-linux-x64; mkdir -p $out
  sed -E "s/^version = \"[^\"]*\"/version = \"$v\"/" Cargo.toml > /tmp/Cargo.toml.$v
  cp Cargo.toml /tmp/Cargo.toml.orig; cp Cargo.lock /tmp/Cargo.lock.orig
  cp /tmp/Cargo.toml.$v Cargo.toml
  CARGO_TARGET_DIR=/target cargo build -q -p hasteclip 2>&1 | grep -E "^error" -A5 || true
  cp /tmp/Cargo.toml.orig Cargo.toml; cp /tmp/Cargo.lock.orig Cargo.lock
  cp /target/debug/hasteclip $out/
  cp crates/app/assets/io.github.kaillr.HasteClip.desktop crates/app/assets/icon.svg $out/
  printf "HasteClip $v for Linux (x86-64)\n" > $out/README.txt
  echo "only-in-$v" > $out/marker-$v.txt
}
build 1.0.0; build 1.1.0
# Install 1.0.0 in the "home folder".
mkdir -p /root/apps && cp -r /tmp/rel-1.0.0/HasteClip-linux-x64 /root/apps/HasteClip
# Publish 1.1.0 on a local server in GitHub's form.
mkdir -p /srv && cd /tmp/rel-1.1.0 && tar -czf /srv/HasteClip-linux-x64.tar.gz HasteClip-linux-x64 && cd /srv && sha256sum HasteClip-linux-x64.tar.gz > HasteClip-linux-x64.tar.gz.sha256
cat > /srv/releases.json <<J
[{"tag_name":"v1.2.0","draft":false,"prerelease":false,"assets":[{"name":"HasteClip-win-Setup.exe","browser_download_url":"x"}]},
 {"tag_name":"v1.1.0","draft":false,"prerelease":false,"assets":[
   {"name":"HasteClip-linux-x64.tar.gz","browser_download_url":"http://127.0.0.1:8000/HasteClip-linux-x64.tar.gz"},
   {"name":"HasteClip-linux-x64.tar.gz.sha256","browser_download_url":"http://127.0.0.1:8000/HasteClip-linux-x64.tar.gz.sha256"}]}]
J
python3 -m http.server 8000 -d /srv >/dev/null 2>&1 &
sleep 1
Xorg :99 -config /xorg.conf -noreset -logfile /tmp/x.log >/dev/null 2>&1 &
sleep 2
export DISPLAY=:99; openbox >/dev/null 2>&1 & sleep 1
export DISPLAY=:99 XDG_SESSION_TYPE=x11 HOME=/root HASTECLIP_LOG=1 HASTECLIP_RELEASES=http://127.0.0.1:8000/releases.json
# Run 1.0.0: it checks after 3 s and downloads; then ask it to quit (SIGTERM) so it installs.
/root/apps/HasteClip/hasteclip > /tmp/run1.txt 2>&1 &
pid=$!
for i in $(seq 1 60); do [ -f /root/apps/.hasteclip-update/HasteClip-linux-x64/README.txt ] && break; sleep 0.5; done
sleep 1
# Close the window like clicking its X: a normal quit.
wmctrl -c HasteClip; for i in $(seq 1 40); do kill -0 $pid 2>/dev/null || break; sleep 0.5; done
kill -0 $pid 2>/dev/null && echo "STILL RUNNING" && kill -TERM $pid
echo "--- app log:"; grep -iE "update|quitting|install|HasteClip 1" /tmp/run1.txt
echo "--- installed folder now:"; ls /root/apps/HasteClip; echo "leftover update folder: $(ls -a /root/apps | grep -c hasteclip-update)"
echo "--- version of the installed app:"; head -1 /root/apps/HasteClip/README.txt
# Start it again: it should be 1.1.0 now.
/root/apps/HasteClip/hasteclip > /tmp/run2.txt 2>&1 &
sleep 5; echo "--- second launch:"; head -1 /tmp/run2.txt; pkill -KILL hasteclip; sleep 1

# Scenario 2: the app is killed after downloading; the next launch installs first.
rm -rf /root/apps/HasteClip && cp -r /tmp/rel-1.0.0/HasteClip-linux-x64 /root/apps/HasteClip; rm -f /root/apps/HasteClip/marker-1.1.0.txt; sleep 1
/root/apps/HasteClip/hasteclip > /tmp/run3.txt 2>&1 &
pid=$!
for i in $(seq 1 60); do [ -f /root/apps/.hasteclip-update/HasteClip-linux-x64/README.txt ] && break; sleep 0.5; done
sleep 1; kill -KILL $pid; sleep 1
echo "--- after a crash: $(head -1 /root/apps/HasteClip/README.txt), pending: $(ls /root/apps/.hasteclip-update 2>/dev/null)"
/root/apps/HasteClip/hasteclip > /tmp/run4.txt 2>&1 &
sleep 6; echo "--- next launch:"; grep -E "installing|^HasteClip 1" /tmp/run4.txt; echo "installed: $(head -1 /root/apps/HasteClip/README.txt)"; pgrep -a hasteclip | head -2; pkill -TERM hasteclip
