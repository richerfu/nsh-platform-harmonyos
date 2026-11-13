import { NativeEditors, EditorSettings, EditorCommand, NshRuntimeContext } from "./NativeEditors";
import { NativeViews, Settings } from "./NativeViews";
import { NativeWebViews, WebSettings, WebCommand } from "./NativeWebViews";

export interface Batch {
  editors: EditorSettings[];
  commands: EditorCommand[];
  removeEditors: number[];
  views: Settings[];
  removeViews: number[];
  webs: WebSettings[];
  webCommands: WebCommand[];
  removeWebs: number[];
}

/** Instance-owned SDK fragments (editors, menus/textures, WebViews) for one UIContext attachment. */
export class NshPlugin {
  private editors: NativeEditors = new NativeEditors();
  private views: NativeViews = new NativeViews();
  private webs: NativeWebViews = new NativeWebViews();

  apply(batch: Batch, context: NshRuntimeContext): void {
    if (!context.isActive()) {
      throw new Error("NSH SDK session is inactive");
    }
    context.getUIContext();
    let failure: Error | undefined;
    const runStep = (step: () => void): void => {
      try {
        step();
      } catch (error) {
        failure ??= error instanceof Error ? error : new Error(String(error));
      }
    };
    runStep((): void => this.editors.remove(batch.removeEditors, context));
    runStep((): void => this.webs.remove(batch.removeWebs, context));
    runStep((): void =>
      this.editors.update(batch.editors, failure ? [] : batch.commands, context),
    );
    runStep((): void => this.views.update(batch.views, context));
    runStep((): void => this.views.remove(batch.removeViews, context));
    runStep((): void =>
      this.webs.update(batch.webs, failure ? [] : batch.webCommands, context),
    );
    if (failure) {
      throw failure;
    }
  }

  dispose(context: NshRuntimeContext): void {
    try {
      this.webs.dispose(context);
    } finally {
      try {
        this.views.dispose(context);
      } finally {
        this.editors.dispose(context);
      }
    }
  }

  async settled(): Promise<void> {
    await this.views.settled();
  }
}
