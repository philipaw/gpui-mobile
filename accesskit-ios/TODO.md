# accesskit-ios — implementation punch list

Per the §10.4 Layer 3 plan in the gem repo's `DESIGN.md`. Tracks
where we are in the implementation across sub-commits, with the
full landed set on top and the remaining work near the bottom.

`accesskit_macos` 0.26 is the canonical template. Its source is at
`~/.cargo/registry/src/.../accesskit_macos-0.26.0/src/`. Files
referenced below are within that path.

## Status (2026-05-10)

**Done:** scaffold (1) + state machine (2a) + query helpers (2b) +
PlatformNode + objc deps (2c) + SubclassingAdapter (3) + gpui-mobile
IosWindow integration (4) + headless end-to-end on iOS Simulator (5,
Path A) + full app bundle + gpui pipeline end-to-end on iOS Simulator
(5b, Path B).

**Remaining:** sub-commit 6 (VoiceOver-on-Simulator validation),
**blocked on a gpui-mobile lifecycle UAF** discovered while bringing
up 5b. The metal view stored at `IosWindow.view: *mut AnyObject` is
not retained by the IosWindow; once the surrounding autorelease pool
drains, the view deallocates and `handle_layout_change` segfaults
trying to send `bounds` to a dangling pointer. 5b currently
`std::process::exit(0)`s after the first TreeUpdate emission to
avoid the crash; sub-commit 6 needs the app to **stay alive** so
VoiceOver can focus + activate elements, which means the UAF has to
be fixed first. See "Sub-commit 6 prereq" below.

All accesskit-ios crate sub-commits live on
`philipaw/gpui-mobile` branch `gem/accesskit-ios-scaffold`. Gem-side
work lives on the gem repo's `main` branch.

## Sub-commit 1 — scaffold (DONE, `773e9d6`)

Lazy-workspaceified the philipaw/gpui-mobile fork; added
`accesskit-ios` as a sibling crate; stubbed `SubclassingAdapter` API
matching accesskit_macos 0.26. Followed by `c7c6695` adding this
TODO.md.

## Sub-commit 2a — `Adapter` state machine (DONE, `b98c426`)

`Adapter` with `Inactive { view, is_view_focused, action_handler }` →
`Placeholder { ... }` → `Active(Rc<Context>)` state machine. Mirrors
`accesskit_macos::Adapter`'s state shape, slimmed to the parts that
have no UIKit dependency: `Context` holds only `tree` +
`action_handler`, no `view` or `platform_nodes` yet. `update_if_active`,
`update_view_focus_state`, and `ensure_initialized` (manual
`Inactive→Active` driver for tests).

Five host-target unit tests cover the Layer-2 failure modes:
`Inactive::update_if_active` is a no-op (factory not invoked),
activation with full tree → Active, activation returning None →
Placeholder, Placeholder → Active on next full update,
post-activation diff (`tree=None`, `nodes=[]`) doesn't panic.
Validated by temporarily forwarding the factory in the Inactive arm;
no-op test failed as expected.

## Sub-commit 2b — query helpers (DONE, `eb730af`)

Three `Adapter` methods that mirror the iOS UIAccessibilityContainer
informal protocol's selectors but stay in pure Rust types so they're
testable without UIKit linkage:

- `accessibility_element_count` → root's filtered child count.
- `accessibility_element_at_index` → `Option<LocalNodeId>`.
- `index_of_accessibility_element` → `Option<isize>`.

All use `accesskit_consumer::common_filter`. Six new unit tests
cover positive/negative/out-of-range/unknown/synthetic-root cases.
Validated by introducing an off-by-one in `position()` → roundtrip
test failed with expected `Some(0) != Some(1)`.

## Sub-commit 2c — `PlatformNode` + objc deps (DONE, `9b0199c`)

UIAccessibilityElement subclass via `objc2::declare_class!` with
`Weak<Context>` + `accesskit_consumer::NodeId` ivars. Overrides
`accessibilityLabel`, `accessibilityTraits`, `accessibilityFrame`,
`accessibilityActivate` — minimum surface for VoiceOver to announce
and tap a Role::Button. Other roles fall through to defaults until a
later commit handles them.

`Context` gains iOS-gated fields: `WeakId<UIView>`, `MainThreadMarker`,
`RefCell<HashMap<NodeId, Id<PlatformNode>>>`. `Adapter` gains three
`*_objc` shim methods returning `*mut NSObject`. `accessibilityFrame`
returns the AccessKit bounding box as-is (assumes tree coords ==
screen coords for the spike); coordinate conversion deferred.

Validation: `cargo check --target aarch64-apple-ios-sim`. 11 host
unit tests still pass unchanged.

Deps added (iOS-target-gated): `objc2 = "0.5"`,
`objc2-foundation = "0.2"` (`NSGeometry` + `NSObject` + `NSString` +
`NSThread`), `objc2-ui-kit = "0.2"` (`UIAccessibility` +
`UIAccessibilityConstants` + `UIAccessibilityContainer` +
`UIAccessibilityElement` + `UIResponder` + `UIView`).

## Sub-commit 3 — `SubclassingAdapter` (DONE, `526abb2`)

Dynamic UIView subclassing that routes UIAccessibility selectors
through the `Adapter` without requiring the host to override the
selectors at compile time. Mirror of
`accesskit_macos::SubclassingAdapter`, ~290 lines.

Per-view-class subclasses cached in a static `Mutex<Vec>`.
`AssociatedObject` NSObject subclass holds the `Adapter` +
`ActivationHandler` + `prev_class` via `declare_class!` ivars.
Selector overrides via `objc2::declare::ClassBuilder`: `superclass`,
`isAccessibilityElement` (forced NO), `accessibilityElementCount`,
`accessibilityElementAtIndex:`, `indexOfAccessibilityElement:`.
`isAccessibilityElement` returns `objc2::runtime::Bool` (Rust `bool`
doesn't impl `Encode` for objc2 method registration).

`objc_setAssociatedObject` + `object_setClass` swap the view's class
in place; `Drop` reverses both.

Validation: `cargo check --target aarch64-apple-ios-sim`. Behavioral
validation deferred to sub-commit 5.

## Sub-commit 4 — gpui-mobile IosWindow integration (DONE, `aae7f7a`)

Repoint gpui-mobile's gpui + gpui_wgpu deps from upstream
`zed-industries/zed @ 5688167` to `philipaw/zed` branch
`gem/a11y-element-trait` so the Layer 1+2 a11y APIs are in scope.
Two unrelated upstream API drifts patched along the way (separate
commit would have been cleaner; bundled with the wiring per
philipaw's call):

- `IosPlatform`: stub `hide_cursor_until_mouse_moves` and
  `is_cursor_visible` (added to the Platform trait upstream).
- `IosDisplay::id`: widen `self.screen as u32` → `u64`
  (DisplayId::new signature widened upstream).

Layer 3 platform integration mirrors `gpui_macos`'s a11y wiring:

- New `ios/a11y.rs`: `A11yState` (initial_update + focus_inverse_map
  + pending_actions), `WindowActivationHandler`, `IosActionHandler`
  (resolves Action::Focus via inverse map → enqueues
  PendingA11yAction::Focus), and a `SendSubclassingAdapter` newtype
  asserting `Send` on the adapter (`Id<UIView>` is `!Send` by design).
- `IosWindow` gains `a11y_adapter:
  Arc<Mutex<SendSubclassingAdapter>>` + `a11y_state:
  Arc<Mutex<A11yState>>` fields. Constructed in `IosWindow::new`
  immediately after the Metal view exists, before the renderer is
  built.
- `impl PlatformWindow for IosWindow` gains
  `take_accessibility_handler` (returns the AccessibilityDrain-
  consuming closure that captures cloned Arcs of state + adapter)
  and `take_pending_a11y_actions` (drains pending_actions on each
  gpui draw).
- `IosWindow::debug_inject_a11y_action` (cfg(debug_assertions))
  bypasses UIKit and synthesizes an ActionRequest into a fresh
  IosActionHandler — Layer 2 lesson carried forward, painful to
  retrofit later.

Validation: `cargo check --target aarch64-apple-ios-sim --workspace`.
accesskit-ios's 11 host tests still pass.

## Sub-commit 5 — headless end-to-end on iOS Simulator (DONE, `2fb6250` gem-side, Path A)

Gem-side spike at `spikes/gpui-ios-a11y-headless/`. Allocates a
bare `UIView` (no UIWindow / UIApplicationMain), constructs
`SubclassingAdapter::new(view, ...)`, feeds in a `TreeUpdate` with
one Role::Button, then calls each iOS UIAccessibility selector
directly via `objc::msg_send!` and asserts the readings match what
we projected.

Output (run via `just spike-gpui-ios-a11y` against booted iPhone
17 simulator):

```
[spike] accessibilityElementCount = 1
[spike] accessibilityElementAtIndex(0) = <non-null PlatformNode pointer>
[spike] PlatformNode.accessibilityLabel = "Hello a11y"
[spike] PlatformNode.accessibilityTraits = 0x1  (UIAccessibilityTraitButton)
[spike] PlatformNode.accessibilityFrame = origin=(0,0) size=(0,0)
[spike] indexOfAccessibilityElement = 0
[spike] action handler fired: action=Click target_node=#2
[spike] accessibilityActivate = true
[spike] SubclassingAdapter dropped (class reverted)
```

Failure-mode validation per CLAUDE.md: temporarily changed the
input label to "OOPS"; the round-trip assertion failed with the
expected mismatch. Reverted before commit.

What this proves: the SubclassingAdapter → AssociatedObject →
Adapter → PlatformNode → AccessKit Tree chain is wired correctly
on real iOS — every selector returns the right value, the action
handler fires synchronously from `accessibilityActivate`, and the
class-swap-and-revert lifecycle is clean.

What this does NOT prove: gpui-mobile's IosWindow a11y wiring (no
IosWindow constructed in the spike); that gpui's element pipeline
actually emits TreeUpdates on iOS; VoiceOver behavior. Those are
sub-commit 5b's job.

## Sub-commit 5b — full iOS app bundle + gpui pipeline (DONE, gem `e1a228c`)

Gem-side spike at `spikes/gpui-ios-a11y-preview/`. Validates
gpui-mobile's IosWindow a11y wiring (sub-commit 4) end-to-end on
real iOS Simulator:

  gpui Element::accessibility() → Window::drain_accessibility_tree
  → IosWindow::take_accessibility_handler closure
  → A11yState.initial_update + SubclassingAdapter::update_if_active

Output via `just spike-gpui-ios-a11y-preview` against booted iPhone
17 sim:

```
[a11y handler] TreeUpdate: 2 nodes  tree=true  focus=#16750113480898009215
[a11y handler]   #16750113480898009215  role=Button  label=Some("hello a11y")
[a11y handler]   #0  role=Window  label=None
[spike-5b] assertion captured; exiting cleanly
```

Same LabeledButton element + handler signature as
`spikes/gpui-mac-preview` — the two outputs are diffable by eye,
proving gpui's a11y pipeline behaves identically across macOS and
iOS at the projection layer.

### Implementation notes

- Standalone workspace at `spikes/gpui-ios-a11y-preview/`, excluded
  from gem's root workspace (gpui pulls zed's wgpu fork, conflicts
  with gem's workspace — same as gpui-mac-preview).
- `[lib] crate-type = ["staticlib"]`. Deps: `gpui` (philipaw/zed
  branch), `accesskit`, gpui-mobile (path = vendor, target-gated to
  iOS). Note: skip `gpui_platform` — its `current_platform()` has
  no iOS arm and fails to compile.
- `src/lib.rs`: exports `gpui_ios_register_app` which stashes a
  window-creation callback via gpui-mobile's
  `set_app_callback`. The callback opens the window + installs the
  a11y handler. After the first emission, calls
  `std::process::exit(0)` — see Sub-commit 6 prereq below for why.
- `ios/main.m`: GpuiAppDelegate that calls `gpui_ios_register_app`
  + `gpui_ios_run_demo` from `didFinishLaunching`, sets up
  CADisplayLink. Does **NOT** forward UIKit lifecycle events
  (`applicationDidBecomeActive` etc.) to gpui-mobile's FFI — those
  panic across `extern "C"` and abort.
- `ios/Info.plist`: bundle id `dev.gem.gpui-ios-a11y-preview`,
  `UILaunchScreen` dict (no separate storyboard / `ibtool` needed,
  iOS 13+).
- `ios/gpui_ios.h`: extern-C declarations for the AppDelegate ↔
  Rust bridge. Includes the lifecycle hooks the AppDelegate intentionally
  doesn't call so the header documents the full surface.
- `build.sh`: hand-rolled bash. cargo build → `xcrun --sdk
  iphonesimulator clang` link with the right framework set →
  `.app` layout → `simctl install` + `simctl launch --console`.
  No XcodeGen / `.xcodeproj`.
- `justfile` recipe: `just spike-gpui-ios-a11y-preview` boots the
  iPhone 17 simulator and runs `build.sh`.

Failure-mode pre-flight per CLAUDE.md: temporarily made
`LabeledButton::accessibility()` return `None`. The `[a11y handler]`
lines disappeared entirely — `drain_accessibility_tree` early-returns
on empty buffer (even stricter than the predicted "1 nodes" miss).
Reverted before commit.

### What 5b proves end-to-end on iOS

- gpui's element pipeline collects nodes from
  `Element::accessibility()` overrides on iOS.
- IosWindow's `take_accessibility_handler` was actually called and
  its closure runs.
- The closure wrote to `A11yState.initial_update` and called
  `SubclassingAdapter::update_if_active` (the `exit(0)` runs from
  inside that same closure).

## Sub-commit 6 — VoiceOver-on-Simulator validation (TODO, blocked)

**Blocked on the gpui-mobile UAF in `IosWindow.view`.** The 5b
spike currently `exit(0)`s after the first a11y emission to dodge
the crash; sub-commit 6 needs the app to **stay alive** so VoiceOver
can focus + activate elements. See "Sub-commit 6 prereq" below for
the fix scope.

Once the app stays alive past the first layout pass:
- `xcrun simctl ui booted accessibility_screen_navigation_enabled YES`
  (or use the Simulator's Settings → Accessibility → VoiceOver toggle)
  to enable VoiceOver in the booted simulator.
- Relaunch the spike via `xcrun simctl launch`.
- Observe VoiceOver announces the LabeledButton with the correct
  role + label when focused.
- Double-tap triggers `accessibilityActivate` (which fires our
  `Action::Click` via `IosActionHandler` — same path sub-commit 5
  already validated end-to-end at the headless layer).

Xcode's iOS Accessibility Inspector (Xcode → Open Developer Tool →
Accessibility Inspector → switch target picker to Simulator) gives
visual verification of the projected tree without needing VoiceOver
gestures.

### Open question for 6 (independent of UAF fix)

Sub-commit 5b's `LabeledButton` returns an AccessKit Node with no
`set_bounds(...)` call. `PlatformNode::accessibilityFrame` returns
ZERO. VoiceOver may not surface elements with zero rects (or may
show them at the screen origin where they're hard to interact with).
If, after the UAF is fixed, VoiceOver doesn't pick up the button,
the next move is to teach `LabeledButton::accessibility()` to call
`node.set_bounds(...)` from the prepaint bounds.
`gpui-mac-preview` probably gets away with no bounds because macOS
VoiceOver is more permissive.

## Sub-commit 6 prereq — fix gpui-mobile's `IosWindow.view` UAF

`IosWindow.view: *mut AnyObject` (window.rs:451) is set to the
output of `[MetalViewClass alloc]; [view initWithFrame:...]` and
stored as a raw pointer. The view has +1 retain count after init
but the IosWindow doesn't take ownership (no `Id` wrap, no
explicit `retain`). Once the surrounding autorelease pool drains,
the view's retain count drops to 0 and it deallocates. Subsequent
`viewDidLayoutSubviews` from UIKit fires
`handle_layout_change`, which sends `bounds` to the now-dangling
pointer and segfaults (Data Abort, "byte read Translation fault").

The defensive guards already in place help but don't cover this:
- `if self.view.is_null() return` (window.rs:1319) — the pointer
  is non-nil, just dangling. Doesn't fire.
- `catch_unwind` around `handle_layout_change` in
  `view_did_layout_subviews` (window.rs:103) — catches Rust panics,
  not segfaults.

**Fix sketch:** make `IosWindow.view` take ownership. Two viable
shapes:
- **(a) `view: objc2::rc::Id<UIView>`.** The objc2 `Id` type
  manages retain/release. Replaces `*mut AnyObject` everywhere
  `view` is read; needs `*const Id::as_ptr(&view) as *mut c_void`
  for the FFI handoff to wgpu. ~30-50 lines of churn but
  type-safe lifetime.
- **(b) Explicit `retain` after init.** Add `let _: () =
  msg_send![view, retain]` in `IosWindow::new` after the alloc/init
  pair. Pair it with a `release` in `Drop`. Smaller diff
  (~10 lines), but the lifetime is implicit / easy to break later.

Same treatment likely needed for `text_input_view` and
`view_controller` if they have the same pattern (verify before
fixing — they may already be retained via parent-view attach).

Validation: after the fix, remove `std::process::exit(0)` from
`spikes/gpui-ios-a11y-preview/src/lib.rs` and confirm the app
stays running. Also remove the `is_null` + `catch_unwind` guards
from gpui-mobile's `window.rs` — they're documented workarounds
for this bug, not real fixes.

## Cost recap

Original §10.4 estimate: 6-8 sittings total for sub-commits 2-6.

Done so far (6 sittings, denser than estimated):
- 1 (scaffold) + 2a/b/c (Adapter + helpers + PlatformNode) +
  3 (SubclassingAdapter) + 4 (IosWindow integration) +
  5 (headless validation, Path A) +
  5b (full app bundle + gpui pipeline, Path B).

Remaining (1.5-2 sittings estimated):
- Sub-commit 6 prereq (gpui-mobile UAF fix on `IosWindow.view`) —
  half-session, mostly Rust + a careful read of the surrounding
  view-controller / wgpu hand-off code in gpui-mobile.
- 6 (VoiceOver validation) — short session, gated on the prereq.

## Lessons carried forward

Don't re-discover these in 6 or in any future iOS adapter work:

- **objc2 versioning**: stay on `objc2 = "0.5"` line +
  `objc2-foundation = "0.2"` + `objc2-ui-kit = "0.2.2"` so we
  parallel `accesskit_macos`'s pin (objc2 0.5 + objc2-app-kit 0.2).
  objc2-uikit feature flags needed:
  `[UIAccessibility, UIAccessibilityConstants,
  UIAccessibilityContainer, UIAccessibilityElement, UIResponder,
  UIView]`. objc2-foundation needs `NSThread` for
  `MainThreadMarker::new()` (without it only `new_unchecked` is
  available, which is `unsafe`).

- **`UIAccessibilityElement` is `MainThreadOnly` mutability**, not
  `InteriorMutable` like macOS's `NSAccessibilityElement`.
  `PlatformNode::new` must take a `MainThreadMarker` parameter.
  `Context` carries the `mtm` so cached PlatformNode allocation
  works without re-checking the thread.

- **`SubclassingAdapter` is `!Send`** because it holds `Id<UIView>`
  (objc Id types are !Send by design). gpui's PlatformWindow
  trait demands `Send` on the take_accessibility_handler closure.
  gpui-mobile wraps in a `SendSubclassingAdapter` newtype with
  `unsafe impl Send`. macOS handles this differently — via `unsafe
  impl Send for MacWindowState {}` on the wrapping struct.

- **iOS subclass needs `isAccessibilityElement` → NO** so UIKit
  treats the host UIView as a container of accesskit-managed
  elements rather than a leaf. Override via ClassBuilder. Method
  signature returns `objc2::runtime::Bool`, NOT Rust `bool` (bool
  doesn't impl `Encode` for objc2 method registration).

- **`Action::Default` doesn't exist in accesskit 0.24**. Just use
  `Action::Click`. `ActionRequest` fields are `target_tree` +
  `target_node`, NOT `target`. Use `node.locate()` to get back
  `(LocalNodeId, TreeId)`.

- **`accesskit_consumer::ChangeHandler` is exported as
  `TreeChangeHandler`** (renamed in the public re-export, while the
  trait declaration in `tree.rs` still uses the bare name). Import
  via `use accesskit_consumer::TreeChangeHandler`.

- **`accesskit_consumer::Tree::new` panics if first `TreeUpdate`
  has `tree: None`**. Cache only the FIRST update for activation.
  This is what `WindowActivationHandler.state.initial_update` is
  for — set once on first drain, never overwritten by subsequent
  diffs.

- **`accesskit_consumer::validate_global` panics if
  `TreeUpdate.focus`** points at the synthetic Window-role root.
  gpui core already picks first non-root child via
  `diff_tree_update` — Layer 3 inherits this for free.

- **macOS's a11y daemon queries the view immediately on window open**;
  iOS likely behaves similarly. Don't crash if `initial_update` is
  None on first activation — the Adapter's Inactive→Placeholder
  fallback handles this. Mirror it in any future variant.

- **The action handler runs on the main thread but without `&mut
  Window/App`**. Cross-thread dispatch is a per-frame queue
  (`PendingA11yAction` + `take_pending_a11y_actions` in gpui core)
  drained at `Window::draw`. Don't try to call `window.focus(...)`
  from the action handler directly.

- **`MacWindow::debug_inject_a11y_action` (cfg-gated) was the
  validation unblocker** for Layer 2 sub-4. `IosWindow::debug_inject_a11y_action`
  (added in sub-commit 4) is the iOS equivalent. Always add this
  hook from day one — retrofitting hurts.

- **gpui-mobile lifecycle FFI (`gpui_ios_did_become_active` etc.)
  panics across `extern "C"`**. `notify_active_status_change` does
  a `RefCell::borrow_mut` on `active_status_callback` that races /
  fails on the spike's call sequence; the panic can't unwind across
  the C boundary, so the process aborts. For spike-quality apps,
  **don't** forward UIKit lifecycle events from the AppDelegate to
  gpui-mobile's FFI — the methods can stay empty and the spike still
  works (sub-commit 5b's main.m demonstrates this).

- **`gpui_platform::current_platform()` has no iOS arm.** It's
  defined to dispatch by OS to a built-in `Platform` impl, but the
  iOS arm is missing — returns `()` and fails to compile. iOS spikes
  hand the platform in directly via
  `gpui::Application::with_platform(gpui_mobile::ios::current_platform(false))`,
  skipping `gpui_platform` entirely.

- **gpui-mobile's `gpui_ios_run_demo` does the entire
  `Application::run` + invoke-callback dance synchronously.**
  Register your window-creation callback via
  `gpui_mobile::ios::ffi::set_app_callback` first; the AppDelegate
  then calls `gpui_ios_register_app` (your function that calls
  `set_app_callback`) followed by `gpui_ios_run_demo`. Don't try to
  manually construct + run the gpui Application from extern-C entry
  points — `IosPlatform::run` only stashes the callback and returns;
  `gpui_ios_run_demo` is what actually invokes it.

- **iOS spike `.app` bundles can be hand-rolled, no XcodeGen.** A
  staticlib + a single Obj-C `main.m` + `Info.plist` (with
  `UILaunchScreen` dict, no separate storyboard) + an `xcrun --sdk
  iphonesimulator clang` link is sufficient for `simctl install` /
  `simctl launch`. The framework set sub-commit 5b's `build.sh`
  uses (UIKit + Foundation + QuartzCore + Metal + MetalKit +
  CoreGraphics + CoreText + CoreFoundation + AVFoundation +
  AudioToolbox + CoreVideo + CoreMedia + VideoToolbox + ImageIO +
  Security + SystemConfiguration + CoreServices + IOSurface +
  `-lc++` + `-liconv`) is a known-good baseline for gpui-mobile
  with default features.

- **gpui-mobile's `IosWindow.view` is not retained** — see
  "Sub-commit 6 prereq" above. Spike apps that need to stay alive
  past the first UIKit layout pass (i.e. anything more than a
  one-shot a11y emission probe) WILL hit this UAF and abort.
  Workaround until fixed: `std::process::exit(0)` from the spike's
  callback after the data you needed has been captured.

- **iOS spike harness can't be a bare `fn main()` for gpui-driven
  workloads** — `IosPlatform::run` requires `UIApplicationMain` to
  be running and the AppDelegate to call back via FFI. Sub-commit 5
  worked around this by going headless (allocate a bare UIView, no
  UIWindow). Sub-commit 5b will need the full app-bundle ceremony.

- **gpui-mobile dep repoint can have unrelated upstream API drift
  to fix.** When repointing to a fresher zed fork commit, expect
  signature changes elsewhere in the Platform trait or DisplayId.
  Keep the repoint commit small and fixup any breakage as part of
  it (or in a separate same-session commit if the user prefers).
