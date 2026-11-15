# Shader rendering and physical-device regression

Device: `2LQ0223A26006180`, HarmonyOS API 26, 1260 × 2720, maximum refresh rate 120 Hz.
The test app is `com.richerfu.elymark`; the user's original Gallery app is separate.

**Acceptance is incomplete.** The current permanent-root-export candidate removes the recorded
fullscreen/exit flash, but regresses ordinary animation rendering and does not meet the 120 FPS
requirement. It is a diagnostic candidate, not an accepted final fix. Reverting to on-demand export
restores the ordinary-animation rate but reproduces the flash, including with the retained-paint
corrections and an output surface visible from creation. No snapshot bridge, fixed-delay handoff,
or application picture cache has been added to hide this gap.

## Root causes and implementation

1. **Enabling fullscreen export removes the displayed source before its first texture exists.**
   ArkUI's `RosenRenderContext::DoTextureExport` calls `RemoveFromTree`. On the on-demand build,
   all four recorded fullscreen-enable operations exposed a white content region for 57.8–74.5 ms.
   This was a real presentation gap. Surface roots now retain one output queue for their lifetime.
   ArkUI exports directly to that queue without sampling effects. When sampling is required, it
   exports to a NativeImage and GLES presents into the same queue. The consumer keeps its last
   buffer until a new producer submits. EGL owns its surface only during GPU presentation; keeping
   the old EGL binding across native producer changes caused repeat-enable corruption.
2. **Custom-node draw commands were not retained across renderer migration.** C custom draws use
   `PaintWrapper::FlushRender` → `RosenRenderContext::FlushContentDrawFunction` →
   `RSCanvasNode::DrawOnNode`. Texture export's `DoFlushModifier` replays retained modifiers, not
   these one-shot commands. Text, canvas, shadows, outlines and header glyphs now use public
   RenderNode/ContentModifier callbacks. Content changes invalidate their modifier property. Bind the modifier to its RenderNode before
   attaching a custom property: Rosen's `RSModifier::AttachProperty` assigns the property's target
   only when the modifier already has a node. The reverse order displayed initial ink but never
   redrew changed text, even though accessibility/test state had updated.
3. **Always copying the native output through GLES regressed ordinary pages.** Direct native
   export to the output queue removes that full-screen copy. Shader programs also adapt vertex
   clip space to the top-left picture convention; a second texture and full-image flip are no
   longer needed for every shader draw.
4. **The rate request and UI frame source were different.** The old implementation applied the
   expected rate to NativeVSync but drove UI animation through ArkUI's post-frame callback.
   A DisplaySync experiment requested 120 but still received 60 Hz on this device. The actual NSH
   animation clock now uses the existing window-associated NativeVSync and UI-thread dispatcher.
   The display ceiling comes from supported refresh rates; explicit application caps are preserved.
   Idle surfaces request no keepalive callbacks.

NativeImage consumes GPU buffers without component snapshots, PixelMap transfers or CPU uploads.
Navigation retains its before picture on the GPU, matching upstream transition semantics.
The old fill error backoff (32/128 ms, three attempts) and its window-failure counters were removed;
render errors are reported and stop that fill until a resource/layer update.

Upstream reference: NSH revision `59f8dcc9eff232a440a536158e0513c2767c0444`, Android
`Pictures.java` / `Box.java` and iOS `shader.rs`. Their content-image producers are platform-specific.
Relevant ArkUI sources are `paint_wrapper.cpp`, `rosen_render_context.cpp`, `rs_canvas_node.cpp`,
`rs_node.cpp`, `rs_custom_modifier.cpp`, and `rs_texture_export.cpp`.
Public references: [native render nodes](https://developer.huawei.com/consumer/cn/doc/harmonyos-references/capi-native-render-h/),
[NativeVSync](https://developer.huawei.com/consumer/en/doc/doccenter-references/api/capi-native-vsync-h/).

## Functional regression

Run the upstream Gallery `tests/shaders.test.tsx` unchanged. Copy this repository's
`tests/gallery/shaders-lifecycle.test.tsx` beside it and import both in `shaders-all.test.tsx`.
The seven cases cover fills, hero content, fullscreen toggles, scrolling, content changes, page/dialog
transitions, and three exits with the effect enabled followed by reentry. Inspect every screenshot;
selector assertions cannot detect corrupted or blank GPU output. The repeated-toggle test also
captures the final enabled state. Test waits cover Gallery's own 600/700 ms transitions; production
rendering uses resource/producer events.

```sh
export NSH_HARMONYOS_SOURCE="$PWD"
export DEVECO_SDK_HOME=/Applications/DevEco-Studio.app/Contents/sdk
export JAVA_HOME=/Applications/DevEco-Studio.app/Contents/jbr/Contents/Home
nsh-platform-harmonyos test .nsh/regression/gallery/tests/shaders-all.test.tsx \
  --device 2LQ0223A26006180 --junit .nsh/gpu/final-shaders-all.xml
```

The previous stable-copy version passed 7/7 physical-device cases and eight recorded toggles without
white frames, but was rejected for its ordinary-page performance cost. The direct-output prototype
passed both upstream cases; recording then found repeat-enable corruption that the EGL lifecycle
change addresses. The pre-correction candidate passed 7/7 selector tests in 540.63 seconds, but screenshot review found
old Morning text after logical state became Noon/Night. This was not accepted: the custom-property
attachment-order correction then passed all seven cases again (519.99 seconds), and screenshots
confirm Noon/Night content updates, repeated toggles, navigation and exit/reentry. Nineteen images
are archived in `.nsh/gpu/property-regression/`. The final recording contains 2,175 frames across four on/off pairs, another enable and a shaded
page exit: zero whole-content white or black frames. The 2,133 frames preceding exit stay within
0.00438 normalized grayscale RMSE of the native/fullscreen references; sampled enabled states
show no repeat-enable corruption. This is visual regression evidence, not an FPS measurement.
Host tests: 165 passed (137 backend + 28 CLI). Native Clippy with warnings denied passed.
The upstream motion-preset assertions also passed on the physical device (45.49 s), with only
the local test timeout expanded from 30 to 120 seconds. The original 30-second run timed out;
no production delay or animation logic was changed to satisfy the test.

The latest optimized rounded-rectangle candidate passed **8/8 cases in 243.54 seconds**: the same
seven functional cases, with their assertions/timeouts unchanged, plus one Hero step-timing
diagnostic. Nineteen screenshots are archived in `rrect-release-regression/`; visual inspection
confirms the Noon/Night content updates. The test CLI used the same source with optimized PNG
dependencies to reduce host-side screenshot encoding time. An earlier run with the slow debug CLI
and debug native library timed out in three of seven cases; that run is preserved, not counted as
a pass. These functional passes do not override the performance failure below.

A final on-demand diagnostic retained all paint/lifecycle fixes and made the presentation surface
visible from creation. Its recording starts before the first fullscreen enable and covers eight
on/off pairs, a ninth enable, and page exit. **All nine enables still expose white content.**
The 4,374-frame variable-rate recording contains 20 white frames in nine runs; the eight warm
runs span approximately 26–43 ms each in the recording. The first run spans 388 ms between its
first white frame and the next nonwhite frame, with sparse capture timestamps, so it is not an
exact measurement of the physical display's blank duration. The single selector test passed,
demonstrating again that logical/UI assertions cannot establish flicker-free presentation.
Both diagnostic source changes were reverted after visual review.

Evidence:

- `.nsh/gpu/on-demand-switches.mp4`, `old-flash-analysis.json`: original white-frame reproduction.
- `.nsh/gpu/fullscreen-flash-before.{png,mp4}`: visual evidence; the excerpt has no audio.
- `.nsh/gpu/stable-surface-all.{log,xml}`, `stable-switches.mp4`: previous stable-copy regression.
- `.nsh/gpu/direct-switches.mp4`, `direct-second-on.png`: rejected eager-EGL prototype corruption.
- `.nsh/gpu/vertex-benchmark/ready-fullscreen.png`: repeat-enable after EGL lifecycle correction.
- `.nsh/gpu/final-shaders-all.{log,xml}`: 7/7 candidate selector tests; visual content-update failure.
- `.nsh/gpu/content-diagnostic-hilog.txt`: new text submitted without its paint callback running.
- `.nsh/gpu/content-property.{log,xml}`: targeted attachment-order regression, passed; both pictures show the new text.
- `.nsh/gpu/final-switches.mp4`, `final-flash-analysis.json`, `final-record-frames/`: final switch/exit recording and visual checks.
- `.nsh/gpu/fullscreen-flash-fixed.mp4`: audio-free final switch/exit excerpt.
- `.nsh/gpu/content-property-{native,full}.png`: Noon in native mode and Night with fullscreen enabled.
- `.nsh/gpu/property-shaders-all.{log,xml}`: complete suite after the attachment-order correction, 7/7 passed (519.99 s).
- `.nsh/gpu/rrect-release-shaders-all.{log,xml}`, `rrect-release-regression/`, `rrect-release-content-review.png`: latest optimized candidate, 8/8 passed (243.54 s), with screenshot review.
- `.nsh/gpu/rrect-shaders-all.{log,xml}`, `rrect-regression-attempt1/`: earlier debug run, 4/7 passed and three timeouts.
- `.nsh/gpu/on-demand-visible-switches.mp4`, `on-demand-visible-flash-analysis.json`, `on-demand-visible-frames/blank.png`: rejected on-demand diagnostic, nine enable flashes despite a passing selector test. The original recording includes audio; use the audio-free excerpt for sharing.
- `.nsh/gpu/on-demand-visible-flash.mp4`: audio-free excerpt of the rejected diagnostic.

## Performance acceptance

Sample actual device FPS separately from recordings and tracing. Screen recordings have variable
frame rate and cannot establish 120 FPS. NativeVSync requests also do not prove delivered frames.
The benchmark uses the same Code rain/Hero viewport, 20 one-second FPS samples per mode, and the
continuous native animations at the top of Animations. No screenshots occur inside the FPS samples.

| Implementation / build | Ordinary shader page | Fullscreen shader | Animations |
| --- | ---: | ---: | ---: |
| Stable-copy, optimized | 114.20 | 105.10 | 60.35 |
| Direct output + vertex adaptation + NativeVSync, debug | 119.90 | 119.75 | 94.40 |
| Direct output + property attachment correction, optimized | 119.25 | 119.90 | 104.25 |
| Native rounded-rectangle paths, debug | — | — | 101.05 |
| Native rounded-rectangle paths, optimized | 119.05 | 119.70 | 105.00 |
| Control: same rounded-rectangle code, root texture export disabled, debug | — | — | 119.95 |

Artifacts: `.nsh/gpu/release-benchmark/`, `.nsh/gpu/native-clock-benchmark/`,
`final-release-benchmark/`, `rrect-benchmark/`, `rrect-release-benchmark/`, `native-tree-benchmark/`.
The optimized Animations trace measures NSH UI work at 0.894 ms median / 1.427 ms p95, but the
texture-export render thread at 9.607 / 11.985 ms. The control confirms that permanent root texture
export adds ordinary-page cost; it is not merely debug-build overhead. The control is diagnostic,
not a flash-safe delivery candidate. Device temperature differs between these runs, so their exact
FPS deltas are not a thermally controlled benchmark.

Preserving native rounded-rectangle paths for circular corners, instead of expanding them into
cubics, lets the platform use its rounded-rectangle blur implementation. Continuous corners still
use the shared upstream geometry. This reduces the measured animation render passes from 25 to
19; the debug texture-export thread measures 7.938 ms median / 10.617 ms p95. This adds no picture
cache or lifecycle state. The optimized geometry-corrected build still averages only 105 FPS on
Animations: its render thread measures 7.991 ms median / 9.665 ms p95, versus 1.259 / 1.687 ms for
NSH UI work. The 20-second FPS sample includes 310 display intervals longer than 12.5 ms. This
candidate fixes the recorded flash but **does not pass the ordinary-animation 120 FPS requirement**.

A diagnostic `RenderGroup` on the retained shadow nodes did not reduce the render cost on this
device (98.35 FPS in the debug build). It was removed; no render-group or application picture cache
was added to the delivered source. The local Rosen texture-export visitor directly traverses the
nodes rather than taking the render-service drawing-group-cache path. Artifact: `group-benchmark/`.

The optimized measurement build
uses unchanged O3 dependencies with `cargo rustc --release -- -C lto=thin -C codegen-units=16` for
its final runner. A no-LTO link cannot reuse these bitcode dependencies with the SDK's older LLVM
linker. Only completed native builds are used for the measurements above.

The latest optimized candidate completed **69/69 Gallery page entries and returns** in one process,
with no detected native errors. The sweep drives the real Gallery NavStack via a local URI test
hook; it adds no keepalive animation to the app. Each page's entry and return are traced. This is
navigation coverage, not a claim that every widget interaction or sustained animation on all pages
has been tested.

Across those traces, NSH UI work exceeded 8.33 ms in 3 of 10,430 callbacks, on three pages. The
private texture-export render thread exceeded that budget in 2,971 of 8,368 callbacks; all 69 pages
had such callbacks, and all had a render-thread p95 above 8.33 ms. This establishes broad rendering
cost in this candidate, but callback duration is not a count of physically dropped display frames.
There are also 16 raw `JANK_FRAME_UNFILTERED` events on 16 pages, all with `filter_type=128`.
That filter's meaning has not been established, so these are retained as diagnostic candidates,
not accepted as confirmed visible-jank counts. Artifacts: `.nsh/gpu/rrect-all-release120/summary.json`,
`aggregate.json`, and the 69 individual compressed traces.

The historical sweeps (`all-debug120/`, `stable-all-debug120/`, `old-system-jank.json`,
`stable-system-jank.json`) precede the direct-output and clock changes. Their raw event totals are
not evidence that every page visibly drops frames and are not acceptance results for this source.
