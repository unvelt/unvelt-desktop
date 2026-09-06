#!/usr/bin/env bash
# Can this machine still read the system now-playing info?
#
# WHY THIS EXISTS
#
# On Windows, SMTC hands any process the current track for any app, browsers
# included. macOS has no public equivalent. The only API that ever gave it is
# MediaRemote.framework, which is private, and Apple is understood to have put
# MRMediaRemoteGetNowPlayingInfo behind an entitlement in macOS 15.4 -- which
# is why nowplaying-cli and the menu-bar now-playing widgets stopped working
# around then.
#
# "Understood to have" is not good enough to design against, so this asks the
# machine. Nothing is installed and nothing is kept: it compiles one file into
# a temp directory, runs it, and deletes it.
#
# RUN IT WITH SOMETHING PLAYING IN A BROWSER TAB, otherwise an empty answer
# tells us nothing -- a gated API and a quiet machine look identical.
#
#     bash tools/probe_mac_nowplaying.sh
#
# V2. The first version made all three calls in one process, so the first
# crash hid every later answer -- and it crashed. This runs each call in its
# own process and reports how each one died, then repeats the whole set
# ad-hoc signed and ad-hoc signed WITH the entitlement. "Crashes unsigned,
# answers signed" and "gated outright" are different conclusions, and only one
# of them leaves something to build.

set -u

echo "macOS:  $(sw_vers -productVersion 2>/dev/null || echo '?')  build $(sw_vers -buildVersion 2>/dev/null || echo '?')  $(uname -m)"
echo

command -v clang >/dev/null 2>&1 || { echo "clang missing: xcode-select --install"; exit 1; }

echo "--- what is making sound right now ---"
# So that an empty now-playing answer can be told apart from a quiet machine.
pgrep -l -f "Google Chrome|Brave Browser|Safari|Spotify|Music|firefox" 2>/dev/null | head -8 || true
echo

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/np.m" <<'OBJC'
#import <Foundation/Foundation.h>
#import <dlfcn.h>

// Every print is flushed. The first version buffered, so when it crashed the
// output stopped one line EARLIER than the code had actually reached, and the
// crash looked like it was in a call the program had already survived.
#define SAY(...) do { printf(__VA_ARGS__); fflush(stdout); } while (0)

typedef void (*InfoFn)(dispatch_queue_t, void (^)(NSDictionary *));
typedef void (*PlayingFn)(dispatch_queue_t, void (^)(Boolean));
typedef void (*ClientFn)(dispatch_queue_t, void (^)(id));
typedef CFStringRef (*BundleFn)(id);

int main(int argc, char **argv) {
  int stage = argc > 1 ? atoi(argv[1]) : 0;
  void *h = dlopen(
    "/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote",
    RTLD_LAZY);
  if (!h) { SAY("DLOPEN FAILED: %s\n", dlerror()); return 2; }

  if (stage == 0) {
    // Symbols only, no calls, so this stage cannot crash. It separates "the
    // framework changed" from "the framework refuses us".
    const char *names[] = {
      "MRMediaRemoteGetNowPlayingInfo",
      "MRMediaRemoteGetNowPlayingApplicationIsPlaying",
      "MRMediaRemoteGetNowPlayingClient",
      "MRNowPlayingClientGetBundleIdentifier",
      "MRMediaRemoteRegisterForNowPlayingNotifications",
    };
    for (int i = 0; i < 5; i++)
      SAY("  %-50s %s\n", names[i], dlsym(h, names[i]) ? "found" : "MISSING");
    return 0;
  }

  if (stage == 1) {
    InfoFn f = (InfoFn)dlsym(h, "MRMediaRemoteGetNowPlayingInfo");
    if (!f) { SAY("  symbol missing\n"); return 3; }
    SAY("  calling MRMediaRemoteGetNowPlayingInfo\n");
    f(dispatch_get_main_queue(), ^(NSDictionary *info) {
      if (!info)       { SAY("  RESULT: (null)\n"); exit(0); }
      if (!info.count) { SAY("  RESULT: (empty dictionary)\n"); exit(0); }
      SAY("  RESULT: %lu keys\n", (unsigned long)info.count);
      for (NSString *k in info) {
        id v = info[k];
        // Artwork is a few hundred KB of image data; never print it.
        if ([v isKindOfClass:NSData.class])
          SAY("    %-50s <%lu bytes>\n", k.UTF8String,
              (unsigned long)((NSData *)v).length);
        else
          SAY("    %-50s %s\n", k.UTF8String, [[v description] UTF8String]);
      }
      exit(0);
    });
  } else if (stage == 2) {
    PlayingFn f = (PlayingFn)dlsym(h, "MRMediaRemoteGetNowPlayingApplicationIsPlaying");
    if (!f) { SAY("  symbol missing\n"); return 3; }
    SAY("  calling MRMediaRemoteGetNowPlayingApplicationIsPlaying\n");
    f(dispatch_get_main_queue(), ^(Boolean playing) {
      SAY("  RESULT: playing = %s\n", playing ? "YES" : "no");
      exit(0);
    });
  } else if (stage == 3) {
    ClientFn f = (ClientFn)dlsym(h, "MRMediaRemoteGetNowPlayingClient");
    BundleFn b = (BundleFn)dlsym(h, "MRNowPlayingClientGetBundleIdentifier");
    if (!f) { SAY("  symbol missing\n"); return 3; }
    SAY("  calling MRMediaRemoteGetNowPlayingClient\n");
    f(dispatch_get_main_queue(), ^(id client) {
      if (!client) { SAY("  RESULT: no client\n"); exit(0); }
      CFStringRef bid = b ? b(client) : NULL;
      // Worth having even with the title withheld: the bundle id says Chrome
      // is the thing playing, which desktop.playing can only infer from the
      // audio device.
      SAY("  RESULT: client = %s\n",
          bid ? [(__bridge NSString *)bid UTF8String] : "(no bundle id)");
      exit(0);
    });
  }

  [NSRunLoop.mainRunLoop runUntilDate:[NSDate dateWithTimeIntervalSinceNow:3]];
  SAY("  NO CALLBACK (waited 3s)\n");
  return 4;
}
OBJC

clang -framework Foundation -o "$work/np" "$work/np.m" 2>"$work/err" || {
  echo "compile failed:"; cat "$work/err"; exit 1; }

cat > "$work/ent.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.mediaremote.send-playback-commands</key><true/>
</dict></plist>
PLIST

run_stage() {
  local rc=0
  "$work/np" "$1" || rc=$?
  if [ "$rc" -gt 128 ]; then
    echo "  ** killed by signal $((rc - 128)) **"
  elif [ "$rc" -ne 0 ]; then
    echo "  ** exit $rc **"
  fi
}

for variant in unsigned adhoc entitled; do
  case "$variant" in
    adhoc)
      codesign -f -s - "$work/np" >/dev/null 2>&1 \
        || { echo "(ad-hoc signing failed)"; continue; } ;;
    entitled)
      codesign -f -s - --entitlements "$work/ent.plist" "$work/np" >/dev/null 2>&1 \
        || { echo "(entitled signing failed -- expected if Apple restricts it)"; continue; } ;;
  esac
  echo "=================== $variant ==================="
  echo "--- symbols ---";            run_stage 0
  echo "--- now playing info ---";   run_stage 1
  echo "--- is playing ---";         run_stage 2
  echo "--- now playing client ---"; run_stage 3
  echo
done
