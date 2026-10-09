
cd /src
# Two "releases": 1.0.0 installed, 1.1.0 on the fake server. Built without ffmpeg (not what's tested).
build() {
  v=$1; out=/tmp/rel-$v/HesteClips-linux-x64; mkdir -p $out
  sed -E "s/^version = \"[^\"]*\"/version = \"$v\"/" Cargo.toml > /tmp/Cargo.toml.$v
  cp Cargo.toml /tmp/Cargo.toml.orig; cp Cargo.lock /tmp/Cargo.lock.orig
  cp /tmp/Cargo.toml.$v Cargo.toml
  CARGO_TARGET_DIR=/target cargo build -q -p hesteclips 2>&1 | grep -E "^error" -A5 || true
  cp /tmp/Cargo.toml.orig Cargo.toml; cp /tmp/Cargo.lock.orig Cargo.lock
  cp /target/debug/hesteclips $out/
  cp crates/app/assets/io.github.kaillr.HesteClips.desktop crates/app/assets/icon.svg $out/
  printf "HesteClips $v for Linux (x86-64)\n" > $out/README.txt
  echo "only-in-$v" > $out/marker-$v.txt
}
build 1.0.0; build 1.1.0
# Install 1.0.0 in the "home folder".
mkdir -p /root/apps && cp -r /tmp/rel-1.0.0/HesteClips-linux-x64 /root/apps/HesteClips
# Publish 1.1.0 on a local server in GitHub's form.
mkdir -p /srv && cd /tmp/rel-1.1.0 && tar -czf /srv/HesteClips-linux-x64.tar.gz HesteClips-linux-x64 && cd /srv && sha256sum HesteClips-linux-x64.tar.gz > HesteClips-linux-x64.tar.gz.sha256
cat > /srv/releases.json <<J
[{"tag_name":"v1.2.0","draft":false,"prerelease":false,"assets":[{"name":"HesteClips-win-Setup.exe","browser_download_url":"x"}]},
 {"tag_name":"v1.1.0","draft":false,"prerelease":false,"assets":[
   {"name":"HesteClips-linux-x64.tar.gz","browser_download_url":"http://127.0.0.1:8000/HesteClips-linux-x64.tar.gz"},
   {"name":"HesteClips-linux-x64.tar.gz.sha256","browser_download_url":"http://127.0.0.1:8000/HesteClips-linux-x64.tar.gz.sha256"}]}]
J
python3 -m http.server 8000 -d /srv >/dev/null 2>&1 &
sleep 1
Xorg :99 -config /xorg.conf -noreset -logfile /tmp/x.log >/dev/null 2>&1 &
sleep 2
export DISPLAY=:99; openbox >/dev/null 2>&1 & sleep 1
export DISPLAY=:99 XDG_SESSION_TYPE=x11 HOME=/root HESTECLIPS_LOG=1 HESTECLIPS_RELEASES=http://127.0.0.1:8000/releases.json
# Run 1.0.0: it checks after 3 s and downloads; then ask it to quit (SIGTERM) so it installs.
/root/apps/HesteClips/hesteclips > /tmp/run1.txt 2>&1 &
pid=$!
for i in $(seq 1 60); do [ -f /root/apps/.hesteclips-update/HesteClips-linux-x64/README.txt ] && break; sleep 0.5; done
sleep 1
# Close the window like clicking its X: a normal quit.
wmctrl -c HesteClips; for i in $(seq 1 40); do kill -0 $pid 2>/dev/null || break; sleep 0.5; done
kill -0 $pid 2>/dev/null && echo "STILL RUNNING" && kill -TERM $pid
echo "--- app log:"; grep -iE "update|quitting|install|HesteClips 1" /tmp/run1.txt
echo "--- installed folder now:"; ls /root/apps/HesteClips; echo "leftover update folder: $(ls -a /root/apps | grep -c hesteclips-update)"
echo "--- version of the installed app:"; head -1 /root/apps/HesteClips/README.txt
# Start it again: it should be 1.1.0 now.
/root/apps/HesteClips/hesteclips > /tmp/run2.txt 2>&1 &
sleep 5; echo "--- second launch:"; head -1 /tmp/run2.txt; pkill -KILL hesteclips; sleep 1

# Scenario 2: the app is killed after downloading; the next launch installs first.
rm -rf /root/apps/HesteClips && cp -r /tmp/rel-1.0.0/HesteClips-linux-x64 /root/apps/HesteClips; rm -f /root/apps/HesteClips/marker-1.1.0.txt; sleep 1
/root/apps/HesteClips/hesteclips > /tmp/run3.txt 2>&1 &
pid=$!
for i in $(seq 1 60); do [ -f /root/apps/.hesteclips-update/HesteClips-linux-x64/README.txt ] && break; sleep 0.5; done
sleep 1; kill -KILL $pid; sleep 1
echo "--- after a crash: $(head -1 /root/apps/HesteClips/README.txt), pending: $(ls /root/apps/.hesteclips-update 2>/dev/null)"
/root/apps/HesteClips/hesteclips > /tmp/run4.txt 2>&1 &
sleep 6; echo "--- next launch:"; grep -E "installing|^HesteClips 1" /tmp/run4.txt; echo "installed: $(head -1 /root/apps/HesteClips/README.txt)"; pgrep -a hesteclips | head -2; pkill -TERM hesteclips
