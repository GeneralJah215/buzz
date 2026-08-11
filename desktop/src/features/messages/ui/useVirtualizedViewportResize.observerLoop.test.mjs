import assert from "node:assert/strict";
import test from "node:test";

class ElementShim {
  constructor() {
    this.children = [];
    this.childNodes = [];
    this.nodeName = "DIV";
    this.tagName = "DIV";
    this.nodeType = 1;
    this.namespaceURI = "http://www.w3.org/1999/xhtml";
  }
  get ownerDocument() {
    return globalThis.document;
  }
  addEventListener() {}
  removeEventListener() {}
  appendChild(child) {
    this.children.push(child);
    this.childNodes.push(child);
    return child;
  }
  removeChild(child) {
    this.children = this.children.filter((current) => current !== child);
    this.childNodes = this.childNodes.filter((current) => current !== child);
    return child;
  }
  insertBefore(child, reference) {
    const index = this.children.indexOf(reference);
    if (index < 0) return this.appendChild(child);
    this.children.splice(index, 0, child);
    this.childNodes.splice(index, 0, child);
    return child;
  }
}

globalThis.document = {
  addEventListener() {},
  createElement: () => new ElementShim(),
  get defaultView() {
    return globalThis.window;
  },
  nodeType: 9,
  removeEventListener() {},
};

const animationFrames = new Map();
let nextAnimationFrameId = 1;
globalThis.requestAnimationFrame = (callback) => {
  const id = nextAnimationFrameId++;
  animationFrames.set(id, callback);
  return id;
};
globalThis.cancelAnimationFrame = (id) => animationFrames.delete(id);
globalThis.HTMLElement = ElementShim;
globalThis.HTMLDivElement = ElementShim;
globalThis.Node = ElementShim;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
process.env.IS_REACT_ACT_ENVIRONMENT = "true";
Object.defineProperty(globalThis, "window", {
  configurable: true,
  value: { addEventListener() {}, removeEventListener() {} },
});
globalThis.window.HTMLIFrameElement = ElementShim;

const resizeObservers = [];
globalThis.ResizeObserver = class {
  constructor(callback) {
    this.callback = callback;
    this.disconnected = false;
    resizeObservers.push(this);
  }
  disconnect() {
    this.disconnected = true;
  }
  observe(target) {
    this.targets = [...(this.targets ?? []), target];
  }
};

import React from "react";
import { act } from "react";
import { createRoot } from "react-dom/client";
import { useVirtualizedViewportResize } from "./useVirtualizedViewportResize.ts";

function flushAnimationFrames() {
  const callbacks = [...animationFrames.values()];
  animationFrames.clear();
  for (const callback of callbacks) callback(performance.now());
}

function resize(observer, width, height) {
  observer.callback([{ contentRect: { height, width } }]);
}

function Harness({ atBottomRef, containerRef, settles }) {
  useVirtualizedViewportResize(containerRef, atBottomRef, () => {
    settles.push(true);
  });
  return null;
}

async function mountHarness({ atBottom = true } = {}) {
  animationFrames.clear();
  resizeObservers.length = 0;
  const container = new ElementShim();
  const settles = [];
  const atBottomRef = { current: atBottom };
  const root = createRoot(new ElementShim());
  await act(async () => {
    root.render(
      React.createElement(Harness, {
        atBottomRef,
        containerRef: { current: container },
        settles,
      }),
    );
  });
  const observer = resizeObservers.find((candidate) =>
    candidate.targets?.includes(container),
  );
  assert.ok(observer, "the viewport is observed");
  return { atBottomRef, observer, root, settles };
}

test("a viewport resize settles on a later frame, never inside the observation pass", async () => {
  const { observer, root, settles } = await mountHarness();

  resize(observer, 800, 600);
  assert.equal(
    settles.length,
    0,
    "no scroll write happens during the ResizeObserver delivery pass",
  );

  flushAnimationFrames();
  assert.equal(settles.length, 1, "the settle lands on the next frame");
  await act(async () => root.unmount());
});

test("a burst of viewport resizes in one pass settles exactly once", async () => {
  const { observer, root, settles } = await mountHarness();

  resize(observer, 800, 600);
  resize(observer, 800, 580);
  resize(observer, 780, 580);
  assert.equal(settles.length, 0);

  flushAnimationFrames();
  assert.equal(settles.length, 1, "one settle per frame, not one per delivery");
  await act(async () => root.unmount());
});

test("a redelivered viewport size does not settle again", async () => {
  const { observer, root, settles } = await mountHarness();

  resize(observer, 800, 600);
  flushAnimationFrames();
  assert.equal(settles.length, 1);

  // The scrollbar toggling back and forth redelivers the same content box.
  // Answering it would re-enter the pin that caused it.
  resize(observer, 800, 600);
  resize(observer, 800, 600.25);
  flushAnimationFrames();
  assert.equal(settles.length, 1, "sub-pixel redelivery is not a reflow");

  // A genuine viewport change still settles.
  resize(observer, 800, 520);
  flushAnimationFrames();
  assert.equal(settles.length, 2, "a real viewport change still re-pins");
  await act(async () => root.unmount());
});

test("a viewport resize does not settle while the reader is up in history", async () => {
  const { observer, root, settles } = await mountHarness({ atBottom: false });

  resize(observer, 800, 600);
  resize(observer, 700, 600);
  flushAnimationFrames();
  assert.equal(settles.length, 0, "reading position is not stolen by a resize");
  await act(async () => root.unmount());
});

test("returning to the bottom re-arms the viewport settle", async () => {
  const { atBottomRef, observer, root, settles } = await mountHarness({
    atBottom: false,
  });

  resize(observer, 800, 600);
  flushAnimationFrames();
  assert.equal(settles.length, 0);

  atBottomRef.current = true;
  resize(observer, 800, 540);
  flushAnimationFrames();
  assert.equal(settles.length, 1);
  await act(async () => root.unmount());
});

test("unmount cancels a scheduled viewport settle", async () => {
  const { observer, root, settles } = await mountHarness();

  resize(observer, 800, 600);
  await act(async () => root.unmount());
  assert.ok(observer.disconnected, "the observer is disconnected");
  assert.equal(animationFrames.size, 0, "the pending frame was cancelled");

  flushAnimationFrames();
  assert.equal(settles.length, 0, "no settle runs after teardown");
});
