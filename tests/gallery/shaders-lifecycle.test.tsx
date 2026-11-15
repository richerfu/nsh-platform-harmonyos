// Run beside the upstream gallery's shaders.test.tsx; see docs/shader-regression.md.
import { expect, open, test } from "nsh/test";

const picture = async (app: Awaited<ReturnType<typeof open>>, name: string) => {
  await app.screenshot(`.nsh/regression/${name}.png`);
};

test("fullscreen effect samples animated shader fills after scrolling", async () => {
  const app = await open("../App.tsx");
  await app.press("label:Shader");
  await app.press("label:Duotone");
  await app.press("2×");
  await picture(app, "fullscreen-animated-fill-0");
  await picture(app, "fullscreen-animated-fill-1");
  await app.press("4×");
  await picture(app, "fullscreen-animated-fill-2");
  await app.press("label:Duotone");
  await app.press("1×");
  await picture(app, "native-animated-fill");
}, 120_000);

test("fullscreen effect preserves hero content while toggling", async () => {
  const app = await open("../App.tsx");
  await app.press("label:Shader");
  await app.press("label:Duotone");
  await app.press("Sweep light");
  await picture(app, "fullscreen-hero");
  await app.press("label:Duotone");
  await app.press("Loop");
  await picture(app, "native-hero");
}, 120_000);

test("fullscreen effect keeps scrolling, content transitions and native state", async () => {
  const app = await open("../App.tsx");
  await app.press("label:Shader");
  await app.press("label:Duotone");
  await picture(app, "fullscreen-on");
  // press scrolls native Scroll to the control, including while its parent is shaded.
  await app.press("Next");
  await app.waitFor("Noon");
  await app.wait(800); // The gallery's wipe lasts 700 ms.
  await picture(app, "fullscreen-content-change");
  await app.press("label:Duotone");
  await picture(app, "fullscreen-off");
  expect(app.shows("Noon")).toBe(true);
  for (let i = 0; i < 5; i++) {
    await app.press("label:Duotone");
    if (i === 4) await picture(app, "repeated-fullscreen-on");
    await app.press("label:Duotone");
  }
  await app.press("Next");
  await app.waitFor("Night");
  await app.wait(800);
  await picture(app, "after-repeated-toggle");
}, 120_000);

test("fullscreen effect survives page and dialog transitions", async () => {
  const app = await open("../App.tsx");
  await app.press("label:Shader");
  await app.press("label:Duotone");
  await app.press("Push a page");
  await app.waitFor("This page came, and goes, through transitionShader");
  await app.wait(800); // The gallery's iris lasts 600 ms.
  await picture(app, "fullscreen-page-open");
  await app.press("Leave the page");
  await app.wait(800);
  expect(app.shows("This page came, and goes, through transitionShader")).toBe(false);
  await picture(app, "fullscreen-page-return");
  await app.press("Open a dialog");
  await app.waitFor("Presented through the iris");
  await app.wait(800);
  await picture(app, "fullscreen-dialog-open");
  await app.press("Close");
  await app.wait(800);
  expect(app.shows("Presented through the iris")).toBe(false);
  await picture(app, "fullscreen-dialog-closed");
  await app.press("label:Duotone");
}, 120_000);

test("leaving a shaded page releases its effect and allows reentry", async () => {
  const app = await open("../App.tsx");
  for (let i = 0; i < 3; i++) {
    await app.press("label:Shader");
    await app.press("label:Duotone");
    await app.back();
    await app.waitFor("Gallery");
    await picture(app, `gallery-after-exit-${i}`);
  }
  await app.press("label:Shader");
  await app.press("Pixelate");
  await app.press("Sharpen");
  await app.press("Next");
  await app.waitFor("Noon");
  await app.wait(800);
  await picture(app, "shader-after-reentry");
}, 120_000);
