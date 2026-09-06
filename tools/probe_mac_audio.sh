#!/usr/bin/env bash
# Why is the Core Audio process list empty on this Mac?
#
# WHAT WE SAW
#
# unvelt reads `kAudioHardwarePropertyProcessObjectList` and, per process,
# `kAudioProcessPropertyIsRunningInput` / `IsRunningOutput` to answer two
# questions: is the microphone in use (and by whom), and is any app making
# sound (desktop.playing). On a real 0.3.2 Mac BOTH came back empty -- no mic
# edge while the mic was on, no desktop.playing while Chrome played a video --
# even though the camera (CoreMediaIO, a different API) and the audio route (a
# different Core Audio property) both worked.
#
# So the suspect is that one property. This asks the machine directly, and
# tests the leading theory: that the process list is gated behind microphone
# permission, which unvelt never requested.
#
# RUN IT WITH THE MIC ON AND AUDIO PLAYING -- e.g. start a Voice Memo AND play
# a YouTube tab -- so an empty list means "cannot see", not "nothing there":
#
#     bash tools/probe_mac_audio.sh
#
# HOW TO READ IT
#
#   list has N process objects, several with IsRunningInput/Output = 1
#       The API works. If unvelt still sees nothing, the bug is in our calls,
#       not the platform -- compare against mac_av.rs.
#
#   list is EMPTY, mic auth = authorized
#       The property is broken/here-unavailable even with permission. Deeper
#       problem; the code path may need replacing.
#
#   list is EMPTY at "not determined", then POPULATES after the prompt
#       Confirmed: the list is microphone-TCC-gated. The fix is that unvelt
#       must hold microphone permission -- an Info.plist usage string plus a
#       one-time request -- and then input AND output detection both come back.

set -u

echo "macOS:  $(sw_vers -productVersion 2>/dev/null || echo '?')  build $(sw_vers -buildVersion 2>/dev/null || echo '?')  $(uname -m)"
echo

command -v clang >/dev/null 2>&1 || { echo "clang missing: xcode-select --install"; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/a.m" <<'OBJC'
#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <AVFoundation/AVFoundation.h>

#define SAY(...) do { printf(__VA_ARGS__); fflush(stdout); } while (0)

static UInt32 u32(AudioObjectID obj, AudioObjectPropertySelector sel) {
  AudioObjectPropertyAddress a = { sel,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
  UInt32 v = 0, size = sizeof(v);
  if (AudioObjectGetPropertyData(obj, &a, 0, NULL, &size, &v) != noErr) return 0;
  return v;
}

static NSString *str(AudioObjectID obj, AudioObjectPropertySelector sel) {
  AudioObjectPropertyAddress a = { sel,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
  CFStringRef s = NULL; UInt32 size = sizeof(s);
  if (AudioObjectGetPropertyData(obj, &a, 0, NULL, &size, &s) != noErr || !s) return nil;
  return (__bridge_transfer NSString *)s;
}

// The whole point of the probe: dump the process list and the running flags.
static int dump(void) {
  AudioObjectPropertyAddress a = { kAudioHardwarePropertyProcessObjectList,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
  UInt32 size = 0;
  OSStatus st = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &a, 0, NULL, &size);
  if (st != noErr) { SAY("  process-list size query failed: OSStatus %d\n", (int)st); return 0; }
  int n = size / sizeof(AudioObjectID);
  SAY("  process list: %d objects\n", n);
  if (n == 0) return 0;
  AudioObjectID *ids = calloc(n, sizeof(AudioObjectID));
  if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &a, 0, NULL, &size, ids) != noErr) {
    SAY("  process-list fetch failed\n"); free(ids); return 0;
  }
  int active = 0;
  for (int i = 0; i < n; i++) {
    UInt32 in  = u32(ids[i], kAudioProcessPropertyIsRunningInput);
    UInt32 out = u32(ids[i], kAudioProcessPropertyIsRunningOutput);
    if (!in && !out) continue;         // only the interesting ones
    active++;
    NSString *bid = str(ids[i], kAudioProcessPropertyBundleID);
    UInt32 pid = u32(ids[i], kAudioProcessPropertyPID);
    SAY("    %-40s pid=%-6u input=%u output=%u\n",
        bid.length ? bid.UTF8String : "(no bundle id)", pid, in, out);
  }
  SAY("  -> %d with input or output running\n", active);
  return n;
}

int main(void) {
  AVAuthorizationStatus s = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
  const char *names[] = { "not determined", "restricted", "denied", "authorized" };
  SAY("microphone TCC: %s\n\n", names[(int)s]);

  SAY("--- before any request ---\n");
  dump();

  if (s == AVAuthorizationStatusNotDetermined) {
    SAY("\n--- requesting microphone access (a prompt should appear) ---\n");
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio
                              completionHandler:^(BOOL granted) {
      SAY("granted: %s\n", granted ? "yes" : "no");
      dispatch_semaphore_signal(sem);
    }];
    dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 60 * NSEC_PER_SEC));
    SAY("\n--- after the request ---\n");
    dump();
  } else {
    SAY("\n(mic already %s -- not re-requesting; if empty above, permission is not the gate)\n", names[(int)s]);
  }
  return 0;
}
OBJC

if ! clang -fobjc-arc -framework Foundation -framework CoreAudio -framework AVFoundation \
     -o "$work/a" "$work/a.m" 2>"$work/err"; then
  echo "compile failed:"; cat "$work/err"; exit 1
fi

# Unsigned first. If permission turns out to be the gate, an ad-hoc signature
# gives it a stable identity to remember the grant against across runs.
echo "=================== unsigned ==================="
"$work/a"
echo
if codesign -f -s - "$work/a" >/dev/null 2>&1; then
  echo "=================== ad-hoc signed ==================="
  "$work/a"
fi
