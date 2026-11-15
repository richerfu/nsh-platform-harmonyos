# The HarmonyOS backend (ArkUI)

`crates/nsh-platform-harmonyos` renders NSH apps on HarmonyOS / OpenHarmony (API 20+, `arm64-v8a`) with ArkUI custom nodes
and lays text out with native `OH_Drawing_Typography`. Each `NshSurface` owns one `NodeContent`, one engine thread and
one QuickJS VM. How apps use it: [the README](../README.md), [embedding.md](embedding.md).

## Building and checking

The backend builds for `aarch64-unknown-linux-ohos` with the OpenHarmony NDK (API 20+); off `target_env = "ohos"` the
crate is empty. The environment cargo needs (the NDK's clang as `CC` and linker, its sysroot for bindgen, its
libclang) is the one `nsh-platform-harmonyos-cli`'s `sdk.rs` sets:

```sh
OHOS_NDK_HOME=… nsh package examples/counter --target harmonyos -v   # the whole path: runner, library, project
cargo check -p nsh-platform-harmonyos --target aarch64-unknown-linux-ohos --features dev   # with the NDK's CC_* / linker set
```

Features:
- Default / `dev`: API 20 baseline (`OH_ArkUI_*` custom nodes, `OH_Drawing_Typography`, `NativeVSync`, `AVPlayer`,
  system `TextInput` / `TextArea`, and `ArkWeb`); `dev` connects to `nsh dev`.
- `api-23`: native accessibility providers, state descriptions and reduced-motion observation.
- `api-26`: includes `api-23` plus named custom accessibility actions.

## On NSH's extension points

- **The environment**: `Environment { os: Os::Android, platform: Some("harmonyos"), .. }` (`app/runtime.rs`). The
  platform is not one of NSH's `Os`; Android's props and behavior (a system back gesture) are the closest, and
  `platform.name` says where the app runs.
- **The linked app**: the runner, a `cdylib` named `nsh_platform_harmonyos` (the N-API module registers under the
  backend's crate name, which the library's name must match), defines `__nsh_platform_harmonyos_app` with the
  backend's `app!` macro (`linked.rs`): the UI sets and, for an app written in Rust, its root from NSH's generated
  `nsh_rust_app`. `NshSurface.runApp()` starts it; the UI sets serve TSX bundles too. A TSX app with Rust crates of
  its own (`nsh.json` `"rust"` without `"root"`) has no `nsh_rust_app`: the runner depends on each crate by path and
  serves `nsh/base`'s UI set, then each crate's `#[native_module]` `registry()` (`app!(sets = [..])`), while the
  bundle is a raw file of the HAP as for any TSX app.
- **Mount items** (`src/render/mount.rs`): `Shader`, `ShaderTransition` and `NavShader` are implemented; `ShaderInput`
  invalidates enclosing effects. NativeImage frame-available events deliver changed native content, including
  scrolling and nested shader fills, without a UI-thread capture request. The terminal-only projections (`TreeOrder`, `ProjectStates`)
  are ignored.
- **Streaming assets**: the video player and web views read large files through `services::assets::StreamAssets`,
  this backend's trait over `nsh_engine::Assets`.

## Architecture

- **One `NshSurface` per embedded view** (`src/app/`): the host creates `new NshSurface(this.getUIContext())` and places
  `surface.content` (`NodeContent`) inside a standard ArkUI `ContentSlot`. `@nsh/harmonyos` binds each window session
  explicitly from the provided `UIContext` (`UIAbilityContext` and `Window`) in a standard `UIAbility` without a custom
  base class or process-global context guessing.
- **Commits, frames and frame-rate arbitration** (`src/render/`, `src/app/frame_rate.rs`, `src/app/runtime.rs`): the
  engine submits each `MountBatch` through a N-API threadsafe function to the ArkUI UI thread. The surface's
  window-associated `NativeVSync` drives demanded NSH animation frames; its callback posts UI work through
  that same dispatcher and never accesses ArkUI off the UI thread. `FrameRate` owns the rate request on
  this actual frame source, combining explicit UI caps with the compositor's animation demand. Uncapped
  animations request the display's maximum supported rate supplied by the window host. Idle surfaces
  request no synthetic keepalive frames. GPU animations keep the shared clock at the panel rate;
  explicit UI caps resume when the last GPU vote ends.
- **Device tests** (`nsh-platform-harmonyos-cli/src/test/`, `src/app/testing.rs`, `src/render/mount/testing.rs`): the
  CLI runs NSH's public `TestRun` and `Agent`, with a `RemoteScene` over the dev protocol. The device's `Model`
  observes mount batches and provides selectors, storage, network mocks and the manual clock. ArkUI answers native
  geometry queries on its UI thread; the CLI injects system input through hdc (`uitest` / `uinput`) and captures the
  marked surface. A separate `.nshtest` bundle and per-open data directories isolate tests from the normal app.
  Quit retires the engine before removing its files; the CLI stops the host and removes its reverse forward.
- **Shaders and composition** (`src/render/shader/`, `src/render/composition/`, `src/interop/`): a per-surface
  `nsh-compositor` worker thread owns an EGL / GLES 3 (`glow`) context and renders outside the ArkUI UI thread across
  three lanes:
  1. **Native ArkUI content**: standard views, transforms, opacity, single blur / backdrop-blur and native blend modes
     are drawn by ArkUI `C-API` nodes into the surface's native texture producer. `Attribute::Blur` and
     `Attribute::BackdropBlur` pass through `native_blur_value(css_radius, scale)`, inverting ArkUI/Rosen's
     physical-pixel `ConvertRadiusToSigma(r) = 0.57735 * r + 0.5` mapping so native blur matches CSS `sigma = radius * scale`.
  2. **GPU effects** (`fill.rs`, `worker.rs` `Composite`): standalone fill shaders render directly into a child C
     `XComponent` surface. Content shaders, filter chains and transitions use a `BuilderNode` texture export
     into a `NativeImage`, then sample its external OES texture on the compositor worker. This follows the upstream
     split between native content recording and GPU effect presentation; there is no component snapshot, PixelMap
     transfer, or CPU readback for these GPU effects. Surface roots retain their output queue throughout their
     lifetime. Without sampling effects, ArkUI exports directly into that queue; the compositor performs no
     full-screen copy. When sampling is needed, the export targets a NativeImage and GLES submits to the same
     output queue. EGL releases its surface when returning the queue to the native producer. Other nodes
     export only for their own effects.
     This permanent-root-export approach remains a candidate: physical-device regression found an ordinary-page
     rendering cost that fails the 120 FPS requirement. See [shader regression](shader-regression.md) for the
     measurements and the rejected on-demand alternative; the flicker result alone does not establish acceptance.
     A stable layout wrapper owns the content parent and sibling presentation surface. Attachment generations and
     worker release acknowledgement bound their lifetimes. Fullscreen shader changes update the root effect’s
     composition while the displayed queue retains its last buffer until the next producer submits. Shader
     programs adapt their vertex clip space to the top-left texture convention, avoiding a separate output-flip
     copy. Navigation retains a GPU texture of the old picture
     for the transition, just as upstream transitions retain their before image.
     Custom text, canvas ink, outlines, shadows and header glyphs use native `RenderNode` content modifiers
     (`paint_node.rs`). C custom-node draw events record one-shot commands via `DrawOnNode`; those recordings are not
     replayed by Rosen’s `DoFlushModifier` when a subtree changes renderer. A content modifier retains its draw
     callback, so renderer migration records the current ink without scroll tracking, subtree repaint loops or
     frame-count delays. Its property is invalidated only by actual content changes. The modifier is attached to its node
     before the property is attached, so Rosen assigns that property a live invalidation target.
  3. **Surface `Scene` path** (`scene.rs`, `draw.rs`): subtrees requiring CSS `mask-image` (`masks[0]`, matching
     upstream mobile backends), multi-filter / backdrop-filter combinations, shader backdrop sampling, or blend modes
     unsupported natively are promoted into a minimal stable `Scene` root (`scene_root_for` keeps non-overflowing
     `mask-image` nodes on their own subtree and stops backdrop walks at the nearest ancestor whose opaque rounded-rect
     background covers the projected sample bounds). `Scene::prepare` builds the composition plan via `paint_plan()` /
     `build_paint_tree()`, commits processed state only on success under an RAII `preparing` guard, marks `Unsupported`
     requirements ineligible with one diagnostic, retries transient errors with bounded backoff (`0` / `100` / `500` ms),
     and unconditionally clears `scene_prepared` / `scene_root` on rollback. `nsh-compositor` reads exported NativeImage textures
     as bounded RGBA bitmaps, composites CPU masks / backdrop filters (`TileMode::Clamp`) and blends, runs any
     attached GLES passes, presents onto the `Scene` root wrapper's custom draw canvas, and logs throttled
     `[NSH] scene-stats` counters (active planes, readback bytes, upload bytes).
     Platform limitation: embedded separate hardware surfaces (`Video` / `ArkWeb` `XComponent`s) inside texture exports depend on
     system surface composition support; their inclusion must be checked on the target OS.
- **Typography** (`src/text/`): the engine thread shapes and measures paragraphs with `ohos-drawing-binding`
  (`FontCollection::shared()`, `Typography`) and hands recorded native drawing commands and selection geometry to the UI
  thread.
- **Controls and media** (`src/input/`, `src/scroll/`, `src/media/`, `src/interop/`): `Input` uses the system
  `TextInput` / `TextArea` BuilderNode; `Scroll` and `List` use native ArkUI `Scroll` (with on-demand `Refresh`);
  `WebView` uses `ArkWeb`; `Video` uses a C `XComponent` surface and an `AVPlayer` worker.
- **Navigation** (`src/navigation/`): HarmonyOS is a native-header platform, as iOS and Android: each `Screen` draws its own
  bar (back button, title) and places only the `headerLeft` / `headerTitle` / `headerRight` items in it; a screen's
  `header` slot (the bar on platforms without a native header) is never shown, as UIKit's and the Android toolbar's
  bars never place it. Screens and modals report `onInsets` as the other backends do: a screen's top inset is 0 below
  a visible, opaque header, else the safe area's top.
- **Hit areas**: `hitSlop` and overflowing children widen `NODE_RESPONSE_REGION`, whose width and height are
  percentages of the node's size (`100` is the node itself), not ratios.
