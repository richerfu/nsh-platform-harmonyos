import { NativeMenuItem, NativeMenuCleanup } from "./NativeMenus";
import { TouchCoordinates, PointerPosting, TouchCancel, NativeRetirement } from "./NativePointer";
import {
  BuilderNode,
  ComponentContent,
  NodeContent,
  NodeController,
  NodeRenderType,
  RenderOptions,
  FrameNode,
} from "@ohos.arkui.node";
import { UIContext } from "@ohos.arkui.UIContext";
import deviceInfo from "@ohos.deviceInfo";
import { NshRuntimeContext } from "./NativeEditors";

interface ViewAction {
  name: string;
  label: string;
}
interface ViewMenu {
  generation: number;
  visible: boolean;
  x: number;
  y: number;
  items: NativeMenuItem[];
}
export interface Settings {
  textureSurface?: string | null;
  textureInsets?: TextureInsets | null;
  targetId: number;
  testId?: string | null;
  width: number;
  height: number;
  hue: number;
  semantic: boolean;
  clickable: boolean;
  disabled: boolean;
  accessible?: boolean | null;
  hidden: boolean;
  label: string;
  hint: string;
  value: string;
  role: string;
  checked?: boolean | null;
  selected: boolean;
  api23: boolean;
  api26: boolean;
  actions: ViewAction[];
  menu?: ViewMenu | null;
}
interface TextureInsets {
  left: number;
  top: number;
  right: number;
  bottom: number;
}
interface ViewParameters {
  entry: ViewEntry;
  settings: Settings;
}
interface MenuParameters {
  entry: ViewEntry;
  menu: ViewMenu;
}
interface MenuLease {
  content: ComponentContent<MenuParameters>;
  generation: number;
  opening: Promise<void>;
  retired: boolean;
  disappeared: boolean;
}
function captureWidth(settings: Settings): number {
  return (
    settings.width + (settings.textureInsets?.left ?? 0) + (settings.textureInsets?.right ?? 0)
  );
}
function captureHeight(settings: Settings): number {
  return (
    settings.height + (settings.textureInsets?.top ?? 0) + (settings.textureInsets?.bottom ?? 0)
  );
}
interface Acknowledgement {
  accepted: boolean;
}
interface CustomAction {
  name: string;
  onAction: () => void;
}
// A structural interface keeps API-26 methods behind both feature and runtime guards.
interface ActionsAttribute {
  accessibilityCustomActions(actions: CustomAction[]): CommonAttribute;
}
interface StateAttribute {
  accessibilityStateDescription(description: string): CommonAttribute;
}
const quarantinedViews: Set<ViewEntry> = new Set();
const roles: Map<string, AccessibilityRoleType> = new Map([
  ["button", AccessibilityRoleType.BUTTON],
  ["checkbox", AccessibilityRoleType.CHECKBOX],
  ["switch", AccessibilityRoleType.SWITCH],
  ["radio", AccessibilityRoleType.RADIO],
  ["slider", AccessibilityRoleType.SLIDER],
  ["image", AccessibilityRoleType.IMAGE],
  ["link", AccessibilityRoleType.HYPERLINK],
  ["list", AccessibilityRoleType.LIST],
  ["grid", AccessibilityRoleType.GRID],
  ["progress", AccessibilityRoleType.PROGRESS],
  ["search", AccessibilityRoleType.SEARCH],
  ["menuitem", AccessibilityRoleType.MENU_ITEM],
  ["text", AccessibilityRoleType.TEXT],
]);
function role(value: string): AccessibilityRoleType | undefined {
  if (value === "") {
    return undefined;
  }
  const result = roles.get(value);
  if (result === undefined) {
    throw new Error("Invalid native wrapper role");
  }
  return result;
}

class ViewModifier implements AttributeModifier<StackAttribute> {
  private entry: ViewEntry;
  private settings: Settings;
  constructor(entry: ViewEntry, settings: Settings) {
    this.entry = entry;
    this.settings = settings;
  }
  applyNormalAttribute(node: StackAttribute): void {
    const s = this.settings;
    const useApi23 =
      s.api23 &&
      deviceInfo.sdkApiVersion >= 23 &&
      typeof (node as StateAttribute).accessibilityStateDescription === "function";
    node
      .id(s.testId ?? `nsh-view-${s.targetId}`)
      .width(s.width)
      .height(s.height)
      .hueRotate(s.hue)
      .clip(false)
      .tabStop(s.menu != null)
      .hitTestBehavior(HitTestMode.Transparent)
      .accessibilityText(s.label)
      .accessibilityDescription(
        useApi23 ? s.hint : [s.hint, s.value].filter((value) => value.length > 0).join(", "),
      )
      .accessibilityLevel(
        s.hidden ? "no-hide-descendants" : !s.semantic || s.accessible === false ? "no" : "yes",
      )
      .accessibilityGroup(s.accessible === true)
      .accessibilitySelected(s.selected);
    node.accessibilityRole(role(s.role));
    node.accessibilityChecked(s.checked === null ? undefined : s.checked);
    node.enabled(!s.disabled).onClick(s.clickable || s.menu != null ? (): void => {} : undefined);
    if (useApi23) {
      (node as StateAttribute).accessibilityStateDescription(s.value);
    }
    if (deviceInfo.sdkApiVersion >= 26 && typeof (node as ActionsAttribute).accessibilityCustomActions === "function") {
      const actions: CustomAction[] = s.actions.map((action: ViewAction): CustomAction => {
        return {
          name: action.label,
          onAction: (): void => {
            this.entry.action(action.name);
          },
        };
      });
      (node as ActionsAttribute).accessibilityCustomActions(actions);
    } else if (s.api26 && s.actions.length > 0) {
      console.warn("[NSH] runtime lacks ArkUI accessibilityCustomActions; degrading custom actions");
    }
  }
}
@Component
struct ViewMenuContent {
  entry!: ViewEntry;
  @Prop menu!: ViewMenu;
  build() {
    Menu() {
      ForEach(
        this.menu.items,
        (item: NativeMenuItem) => {
          MenuItem({ content: item.label })
            .onClick((): void => {
              this.entry.action(item.name, "menu", this.menu.generation);
            });
        },
        (item: NativeMenuItem): string => item.name,
      )
    }.padding(8);
  }
}
@Builder
function menuContent(params: MenuParameters) {
  ViewMenuContent({ entry: params.entry, menu: params.menu });
}
@Builder
function nativeView(params: ViewParameters) {
  Stack({ alignContent: Alignment.TopStart }) {
    ContentSlot(params.entry.slot)
  }
  .width(params.settings.width)
  .height(params.settings.height)
  .attributeModifier(new ViewModifier(params.entry, params.settings))
  .onAccessibilityActionIntercept(
    (action: AccessibilityAction): AccessibilityActionInterceptResult => {
      if (action === AccessibilityAction.ACCESSIBILITY_CLICK && params.entry.action("activate")) {
        return AccessibilityActionInterceptResult.ACTION_INTERCEPT;
      }
      return AccessibilityActionInterceptResult.ACTION_CONTINUE;
    },
  );
}
class ViewController extends NodeController {
  private node?: BuilderNode<[ViewParameters]>;
  constructor(node: BuilderNode<[ViewParameters]>) {
    super();
    this.node = node;
  }
  makeNode(_context: UIContext): FrameNode | null {
    return this.node?.getFrameNode() ?? null;
  }
  clear(): void {
    this.node = undefined;
    this.rebuild();
  }
}
@Observed
class ViewState {
  settings: Settings;
  constructor(settings: Settings) {
    this.settings = settings;
  }
}
// Custom roots preserve ArkUI's native export boundary. Observed settings refresh
// their attributes without rebuilding the slot or changing its native ownership.
@Component
struct SourceRoot {
  entry!: ViewEntry;
  @ObjectLink state!: ViewState;
  build() {
    Stack({ alignContent: Alignment.TopStart }) {
      nativeView({ entry: this.entry, settings: this.state.settings })
    }
    .width(captureWidth(this.state.settings))
    .height(captureHeight(this.state.settings))
    .padding(this.state.settings.textureInsets ?? { left: 0, top: 0, right: 0, bottom: 0 })
    .clip(false)
    .hitTestBehavior(HitTestMode.Transparent);
  }
}
@Builder
function sourceContent(params: ViewParameters) {
  SourceRoot({ entry: params.entry, state: params.entry.state });
}
@Component
struct TextureRoot {
  entry!: ViewEntry;
  @ObjectLink state!: ViewState;
  build() {
    NodeContainer(this.entry.sourceController!)
      .width(captureWidth(this.state.settings))
      .height(captureHeight(this.state.settings))
      .clip(false)
      .hitTestBehavior(HitTestMode.Transparent);
  }
}
@Builder
function textureContent(params: ViewParameters) {
  TextureRoot({ entry: params.entry, state: params.entry.state });
}
@Component
struct TextureContainer {
  entry!: ViewEntry;
  @ObjectLink state!: ViewState;
  build() {
    Stack({ alignContent: Alignment.TopStart }) {
      NodeContainer(this.entry.textureController!)
        .width(captureWidth(this.state.settings))
        .height(captureHeight(this.state.settings))
        .clip(false)
        .hitTestBehavior(HitTestMode.Transparent)
        .onTouch((event: TouchEvent): void => {
          this.entry.postMouseTouch(event);
        })
    }
    .width(captureWidth(this.state.settings))
    .height(captureHeight(this.state.settings))
    .clip(false)
    .hitTestBehavior(HitTestMode.Transparent);
  }
}
@Component
struct DirectRoot {
  entry!: ViewEntry;
  @ObjectLink state!: ViewState;
  build() {
    nativeView({ entry: this.entry, settings: this.state.settings });
  }
}
@Builder
function viewContent(params: ViewParameters) {
  if (params.entry.textureNode) {
    TextureContainer({ entry: params.entry, state: params.entry.state });
  } else {
    DirectRoot({ entry: params.entry, state: params.entry.state });
  }
}
class ViewEntry {
  readonly state: ViewState;
  readonly slot: NodeContent = new NodeContent();
  readonly textureNode?: BuilderNode<[ViewParameters]>;
  readonly sourceNode?: BuilderNode<[ViewParameters]>;
  readonly sourceController?: ViewController;
  readonly textureController?: ViewController;
  private node: BuilderNode<[ViewParameters]>;
  private context: NshRuntimeContext;
  private ui: UIContext;
  private menus: NativeMenuCleanup;
  private closed: boolean = false;
  private attached: boolean = false;
  private retiredNative: boolean = false;
  private nodeDisposed: boolean = false;
  private textureDisposed: boolean = false;
  private sourceDisposed: boolean = false;
  private sdkBusy: boolean = false;
  private disposePending: boolean = false;
  private retirementFailure?: Error;
  private menuLease?: MenuLease;
  private pointerPosting: PointerPosting;
  private retirement: NativeRetirement = new NativeRetirement();
  constructor(
    settings: Settings,
    context: NshRuntimeContext,
    menus: NativeMenuCleanup,
    posting: PointerPosting,
  ) {
    this.state = new ViewState(settings);
    this.menus = menus;
    this.context = context;
    this.ui = context.getUIContext();
    this.pointerPosting = posting;
    const options: RenderOptions | undefined =
      settings.textureSurface == null
        ? undefined
        : {
            type: NodeRenderType.RENDER_TYPE_TEXTURE,
            surfaceId: settings.textureSurface,
            selfIdealSize: { width: captureWidth(settings), height: captureHeight(settings) },
          };
    if (options !== undefined) {
      this.textureNode = new BuilderNode<[ViewParameters]>(this.ui, options);
      this.sourceNode = new BuilderNode<[ViewParameters]>(this.ui);
      this.sourceController = new ViewController(this.sourceNode);
      this.textureController = new ViewController(this.textureNode);
    }
    this.node = new BuilderNode<[ViewParameters]>(this.ui);
  }
  get settings(): Settings {
    return this.state.settings;
  }
  isClosed(): boolean {
    return this.closed;
  }
  belongsTo(context: NshRuntimeContext): boolean {
    return this.context === context;
  }
  private focusTargetNode(): FrameNode | null {
    const root = this.node.getFrameNode();
    let cursor: FrameNode | null = root;
    let depth = 0;
    while (cursor && cursor.getNodeType() !== "Stack" && depth < 8) {
      cursor = cursor.getFirstChild();
      depth += 1;
    }
    return cursor ?? root;
  }
  private abortCreate(): void {
    this.closed = true;
    try {
      if (this.menuLease) {
        this.retireMenu(this.menuLease);
        this.menuLease = undefined;
      }
    } catch (_) {}
    try {
      this.disposeBuilders();
    } catch (error) {
      console.error(`[NSH] failed to clean up aborted wrapper ${this.settings.targetId}: ${String(error)}`);
    }
  }
  create(): void {
    this.sdkBusy = true;
    let stage: string = "build";
    try {
      const params: ViewParameters = { entry: this, settings: this.settings };
      if (this.textureNode) {
        this.sourceNode?.build(wrapBuilder(sourceContent), params, {
          nestingBuilderSupported: true,
        });
        if (this.closed) {
          throw new Error("Native wrapper creation was cancelled");
        }
        this.textureNode.build(wrapBuilder(textureContent), params, {
          nestingBuilderSupported: true,
        });
      }
      if (this.closed) {
        throw new Error("Native wrapper creation was cancelled");
      }
      this.node.build(wrapBuilder(viewContent), params, { nestingBuilderSupported: true });
      if (this.closed || !this.context.isActive()) {
        throw new Error("Native wrapper creation was cancelled");
      }
      stage = "attach";
      const accepted = this.context.attachNativeSubtree(
        this.settings.targetId,
        this.node.getFrameNode(),
        this.slot,
      );
      if (!accepted) {
        throw new Error("Native wrapper attachment was rejected");
      }
      this.attached = true;
      this.syncMenu();
    } catch (error) {
      if (this.attached) {
        try {
          this.dispose();
        } catch (_) {}
      } else {
        this.abortCreate();
      }
      throw new Error(`Native wrapper ${this.settings.targetId} ${stage} failed: ${String(error)}`);
    } finally {
      this.sdkBusy = false;
      if (this.disposePending) {
        this.disposePending = false;
        this.disposeBuilders();
      }
    }
  }
  update(settings: Settings): void {
    if (this.closed) {
      throw new Error("Native wrapper is already closed");
    }
    if (settings.targetId !== this.settings.targetId) {
      throw new Error("Native wrapper target cannot change");
    }
    this.state.settings = settings;
    const params: ViewParameters = { entry: this, settings: settings };
    this.sdkBusy = true;
    try {
      this.node.update(params);
    } finally {
      this.sdkBusy = false;
      if (this.disposePending) {
        this.disposePending = false;
        this.disposeBuilders();
      }
    }
    if (!this.closed) {
      this.syncMenu();
    }
  }
  private syncMenu(): void {
    try {
      this.applyMenu();
    } catch (error) {
      console.error(`[NSH] native menu construction failed: ${String(error)}`);
      const lease = this.menuLease;
      this.menuLease = undefined;
      if (lease) {
        this.retireMenu(lease);
      }
      this.action("", "menu-dismiss", this.settings.menu?.generation);
    }
  }
  private applyMenu(): void {
    const menu = this.settings.menu;
    const previous = this.menuLease;
    if (this.closed || menu?.visible !== true) {
      this.menuLease = undefined;
      if (previous) {
        this.retireMenu(previous);
      }
      return;
    }
    if (previous?.generation === menu.generation) {
      previous.content.update({ entry: this, menu: menu });
      return;
    }
    if (previous) {
      this.retireMenu(previous);
    }
    const ui = this.ui;
    const frame = this.focusTargetNode() ?? this.node.getFrameNode();
    if (!frame) {
      return;
    }
    // C custom nodes have a disabled focus type. Their native Stack wrapper
    // owns selection focus, so keys bubble only inside this surface and the
    // menu restores that focus without taking it back from a host control.
    ui.getFocusController().requestFocus(
      this.settings.testId ?? `nsh-view-${this.settings.targetId}`,
    );
    const content = new ComponentContent<MenuParameters>(
      ui,
      wrapBuilder(menuContent),
      { entry: this, menu: menu },
      { nestingBuilderSupported: true },
    );
    const lease: MenuLease = {
      content: content,
      generation: menu.generation,
      opening: Promise.resolve(),
      retired: false,
      disappeared: false,
    };
    this.menuLease = lease;
    lease.opening = ui
      .getPromptAction()
      .openMenu(content, { id: frame.getUniqueId() }, this.menuOptions(lease));
    lease.opening.catch((error: Error): void => {
      lease.disappeared = true;
      console.error(`[NSH] native menu open failed: ${String(error)}`);
      if (this.menuLease === lease) {
        this.menuLease = undefined;
        this.action("", "menu-dismiss", lease.generation);
      }
      this.retireMenu(lease);
    });
  }
  private retireMenu(lease: MenuLease): void {
    if (lease.retired) {
      return;
    }
    lease.retired = true;
    this.menus.retire(
      lease,
      lease.opening,
      (): Promise<void> =>
        lease.disappeared ? Promise.resolve() : this.ui.getPromptAction().closeMenu(lease.content),
      (): void => lease.content.dispose(),
    );
  }

  postMouseTouch(event: TouchEvent): void {
    const pointer = event.changedTouches[0]?.id;
    const down = event.type === TouchType.Down;
    const terminal = event.type === TouchType.Up || event.type === TouchType.Cancel;
    const owner = pointer === undefined ? undefined : this.pointerPosting.owners.get(pointer);
    const target = this.sourceNode;
    if (
      this.closed ||
      this.pointerPosting.busy ||
      !target ||
      !this.context.isActive() ||
      event.source !== SourceType.Mouse ||
      pointer === undefined ||
      (down ? owner !== undefined : owner?.owner !== String(this.settings.targetId))
    ) {
      return;
    }
    const offset = target.getFrameNode()?.getPositionToWindow();
    if (!offset) {
      return;
    }
    const coordinates = new TouchCoordinates(event);
    this.pointerPosting.busy = true;
    try {
      coordinates.apply(this.ui, offset.x, offset.y);
      const accepted = target.postInputEvent(event);
      if (accepted) {
        if (!terminal) {
          const snapshot = new TouchCancel(event);
          if (owner) {
            owner.snapshot = snapshot;
          } else {
            this.pointerPosting.owners.set(pointer, {
              owner: String(this.settings.targetId),
              snapshot: snapshot,
            });
          }
        }
        event.stopPropagation();
      } else if (down && !this.closed && this.context.isActive()) {
        // Even a miss records DOWN in ArkUI. Close that attempt before another export tries.
        coordinates.cancel();
        target.postInputEvent(event);
      }
    } finally {
      if (terminal) {
        this.pointerPosting.owners.delete(pointer);
      }
      this.pointerPosting.finish(coordinates);
    }
  }
  menuOptions(lease: MenuLease): MenuOptions {
    const menu = this.settings.menu;
    return {
      placement: Placement.TopLeft,
      offset: { x: menu?.x ?? 0, y: -(menu?.y ?? 0) },
      showInSubWindow: false,
      onDisappear: (): void => {
        lease.disappeared = true;
        // Every popup retains its own generation, including during replacement.
        const generation = menu?.generation;
        if (generation !== undefined) {
          this.action("", "menu-dismiss", generation);
        }
      },
    };
  }
  action(name: string, kind?: string, generation?: number): boolean {
    if (
      this.closed ||
      !this.context.isActive() ||
      (kind !== undefined && generation !== this.settings.menu?.generation)
    ) {
      return false;
    }
    return this.context.viewAction({
      targetId: this.settings.targetId,
      action: name,
      kind: kind,
      generation: generation,
    });
  }
  dispose(cleanup: NshRuntimeContext = this.context): void {
    if (this.closed && !this.retirementFailure) {
      return;
    }
    this.closed = true;
    this.retirementFailure = undefined;
    try {
      if (this.menuLease) {
        this.retireMenu(this.menuLease);
        this.menuLease = undefined;
      }
      this.retirement.detachThenDispose(
        (): void => {
          if (this.attached && !this.retiredNative) {
            if (!cleanup.retireNativeNode(this.settings.targetId)) {
              throw new Error("Native wrapper retirement was rejected");
            }
            this.retiredNative = true;
          }
        },
        (): void => this.disposeBuilders(),
        this.pointerPosting,
      );
      quarantinedViews.delete(this);
    } catch (error) {
      quarantinedViews.add(this);
      this.retirementFailure = new Error(`Native wrapper retirement failed: ${String(error)}`);
      throw this.retirementFailure;
    }
  }
  private disposeBuilders(): void {
    if (this.sdkBusy) {
      this.disposePending = true;
      return;
    }
    let failure: Error | undefined;
    const step = (fn: () => void): void => {
      try {
        fn();
      } catch (error) {
        failure ??= error instanceof Error ? error : new Error(String(error));
      }
    };
    step((): void => {
      this.pointerPosting.owners.forEach((stream, pointer: number): void => {
        if (stream.owner !== String(this.settings.targetId)) {
          return;
        }
        this.pointerPosting.owners.delete(pointer);
        if (!this.sourceDisposed && this.sourceNode?.getFrameNode()) {
          this.pointerPosting.busy = true;
          try {
            this.sourceNode.postInputEvent(stream.snapshot as TouchEvent);
          } finally {
            this.pointerPosting.finish();
          }
        }
      });
    });
    // NodeContainer retains the node returned by its controller. Detach
    // those edges before disposing the builders, including texture exports.
    step((): void => this.textureController?.clear());
    step((): void => this.sourceController?.clear());
    if (!this.nodeDisposed) {
      step((): void => {
        this.node.dispose();
        this.nodeDisposed = true;
      });
    }
    if (this.textureNode && !this.textureDisposed) {
      step((): void => {
        this.textureNode!.dispose();
        this.textureDisposed = true;
      });
    }
    if (this.sourceNode && !this.sourceDisposed) {
      step((): void => {
        this.sourceNode!.dispose();
        this.sourceDisposed = true;
      });
    }
    if (failure) {
      quarantinedViews.add(this);
      this.retirementFailure = failure;
      console.error(`[NSH] wrapper disposal failed: ${String(failure)}`);
      throw failure;
    }
    quarantinedViews.delete(this);
  }
}
export class NativeViews {
  private entries: Map<number, ViewEntry> = new Map();
  private menus: NativeMenuCleanup = new NativeMenuCleanup();
  private pointerPosting: PointerPosting = new PointerPosting();
  private retryQuarantined(context: NshRuntimeContext): void {
    for (const view of Array.from(quarantinedViews)) {
      if (!view.belongsTo(context)) {
        continue;
      }
      try {
        view.dispose(context);
      } catch (_) {}
    }
  }
  remove(ids: number[], context: NshRuntimeContext): void {
    this.retryQuarantined(context);
    let failure: Error | undefined;
    for (const id of ids) {
      const entry = this.entries.get(id);
      if (!entry) {
        continue;
      }
      try {
        entry.dispose(context);
        this.entries.delete(id);
      } catch (error) {
        failure ??= error instanceof Error ? error : new Error(String(error));
      }
    }
    if (failure) {
      throw failure;
    }
  }
  update(states: Settings[], context: NshRuntimeContext): void {
    this.retryQuarantined(context);
    let failure: Error | undefined;
    for (const state of states) {
      try {
        if (!context.isActive()) {
          throw new Error("NSH SDK session is inactive");
        }
        let entry = this.entries.get(state.targetId);
        if (entry && entry.isClosed()) {
          this.entries.delete(state.targetId);
          entry = undefined;
        }
        if (entry && entry.settings.textureSurface !== state.textureSurface) {
          throw new Error("Texture replacement requires a fresh native target");
        }
        if (entry) {
          entry.update(state);
        } else {
          const created = new ViewEntry(
            state,
            context,
            this.menus,
            this.pointerPosting,
          );
          created.create();
          this.entries.set(state.targetId, created);
        }
      } catch (error) {
        failure ??= error instanceof Error ? error : new Error(String(error));
      }
    }
    if (failure) {
      throw failure;
    }
  }
  async settled(): Promise<void> {
    await this.menus.settled();
  }
  dispose(context: NshRuntimeContext): void {
    let failure: Error | undefined;
    for (const id of Array.from(this.entries.keys())) {
      const entry = this.entries.get(id)!;
      try {
        entry.dispose(context);
        this.entries.delete(id);
      } catch (error) {
        failure ??= new Error(String(error));
      }
    }
    for (const view of Array.from(quarantinedViews)) {
      if (!view.belongsTo(context)) {
        continue;
      }
      try {
        view.dispose(context);
      } catch (error) {
        failure ??= new Error(String(error));
      }
    }
    if (failure) {
      throw failure;
    }
  }
}
