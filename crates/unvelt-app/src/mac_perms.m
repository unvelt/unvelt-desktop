// The two macOS permissions unvelt cannot collect without, requested the only
// way macOS allows: from code. Neither the microphone nor the camera list in
// System Settings has an "add" button, so a permission the app never asks for
// can never be granted by hand -- the request has to come from here.
#import <Foundation/Foundation.h>
#import <AVFoundation/AVFoundation.h>
#import <ApplicationServices/ApplicationServices.h>
#include <stdbool.h>

// Microphone. unvelt never records; it reads Core Audio's per-process
// "is running input"/"is running output" flags, and a process without this
// grant sees them all as zero -- which is exactly why mic use and
// desktop.playing were both blank on a real Mac while a call and a video ran.
// Fire-and-forget: once granted, the collector reads it on its next poll, so
// the completion handler has nothing to do.
void unvelt_request_mic(void) {
    @autoreleasepool {
        if ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]
                == AVAuthorizationStatusNotDetermined) {
            [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio
                                     completionHandler:^(BOOL granted) { (void)granted; }];
        }
    }
}

// Accessibility. The front window's TITLE (not its app name, which needs
// nothing) is read through System Events' accessibility and stays empty until
// unvelt is trusted here. With the prompt option set, macOS shows the grant
// dialog only when not already trusted, and simply returns true when it is --
// so this is safe to call on every launch. Tied to the code signature, so an
// unsigned build asks again after each update; that is inherent, not a bug.
bool unvelt_prompt_accessibility(void) {
    @autoreleasepool {
        const void *keys[] = { kAXTrustedCheckOptionPrompt };
        const void *vals[] = { kCFBooleanTrue };
        CFDictionaryRef opts = CFDictionaryCreate(NULL, keys, vals, 1,
            &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        bool trusted = AXIsProcessTrustedWithOptions(opts);
        CFRelease(opts);
        return trusted;
    }
}
