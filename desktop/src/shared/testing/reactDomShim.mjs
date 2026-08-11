/**
 * Minimal DOM shim for mounting real React trees under `node --test`.
 *
 * Lifted verbatim in behaviour from the copy inlined in
 * `useObserverEvents`-adjacent tests (originally
 * MessageComposerDraftImagePersist.test.mjs) and hoisted here so the BUG-067
 * subscriber-scoping regressions can mount production hooks without a fourth
 * copy of 150 lines of shim.
 *
 * Not a test file (no `.test.mjs` suffix), so the runner does not pick it up.
 */

class MinimalEventTarget {
  constructor() {
    this._listeners = {};
  }
  addEventListener(type, fn) {
    if (!this._listeners[type]) this._listeners[type] = [];
    this._listeners[type].push(fn);
  }
  removeEventListener(type, fn) {
    if (this._listeners[type]) {
      this._listeners[type] = this._listeners[type].filter((f) => f !== fn);
    }
  }
  dispatchEvent(e) {
    for (const fn of this._listeners[e.type] ?? []) fn(e);
    return true;
  }
}

class MinimalNode extends MinimalEventTarget {
  constructor(tagName) {
    super();
    this.tagName = tagName;
    this.children = [];
    this.childNodes = [];
    this.style = {};
    this.nodeType = 1;
    this.parentNode = null;
    this._attributes = {};
  }
  get ownerDocument() {
    return globalThis.document;
  }
  get firstChild() {
    return this.children[0] ?? null;
  }
  get lastChild() {
    return this.children[this.children.length - 1] ?? null;
  }
  get nextSibling() {
    return null;
  }
  get nodeValue() {
    return this._nodeValue ?? null;
  }
  set nodeValue(value) {
    this._nodeValue = value;
  }
  setAttribute(name, value) {
    this._attributes[name] = value;
  }
  removeAttribute(name) {
    delete this._attributes[name];
  }
  getAttribute(name) {
    return this._attributes[name] ?? null;
  }
  appendChild(child) {
    this.children.push(child);
    this.childNodes.push(child);
    child.parentNode = this;
    return child;
  }
  removeChild(child) {
    this.children = this.children.filter((c) => c !== child);
    this.childNodes = this.childNodes.filter((c) => c !== child);
    return child;
  }
  insertBefore(newNode, refNode) {
    if (!refNode) return this.appendChild(newNode);
    const i = this.children.indexOf(refNode);
    if (i < 0) return this.appendChild(newNode);
    this.children.splice(i, 0, newNode);
    this.childNodes.splice(i, 0, newNode);
    newNode.parentNode = this;
    return newNode;
  }
  contains(node) {
    if (!node) return false;
    return this === node || this.children.some((c) => c?.contains?.(node));
  }
}

class MinimalDocument extends MinimalEventTarget {
  constructor() {
    super();
    this.nodeType = 9;
  }
  createElement(tagName) {
    return new MinimalNode(tagName);
  }
  createTextNode(value) {
    const n = new MinimalNode("#text");
    n.nodeValue = value;
    n.nodeType = 3;
    return n;
  }
  createComment(value) {
    const n = new MinimalNode("#comment");
    n.nodeValue = value;
    n.nodeType = 8;
    return n;
  }
  get body() {
    if (!this._body) this._body = this.createElement("body");
    return this._body;
  }
  get activeElement() {
    return null;
  }
  contains(node) {
    return node != null;
  }
}

/** Collect the concatenated text of a shim node subtree. */
export function textContentOf(node) {
  if (!node) return "";
  if (node.nodeType === 3) return node.nodeValue ?? "";
  return (node.children ?? []).map(textContentOf).join("");
}

export function installDOMShim() {
  if (globalThis.document instanceof MinimalDocument) return;

  globalThis.document = new MinimalDocument();
  globalThis.HTMLIFrameElement = MinimalNode;
  globalThis.HTMLElement = MinimalNode;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  process.env.IS_REACT_ACT_ENVIRONMENT = "true";

  if (typeof globalThis.window === "undefined") {
    Object.defineProperty(globalThis, "window", {
      value: globalThis,
      configurable: true,
    });
  }
  if (!Object.getOwnPropertyDescriptor(globalThis, "navigator")?.value) {
    Object.defineProperty(globalThis, "navigator", {
      value: { userAgent: "node" },
      configurable: true,
    });
  }
  globalThis.MutationObserver = class {
    observe() {}
    disconnect() {}
    takeRecords() {
      return [];
    }
  };
  globalThis.requestAnimationFrame = (fn) => setTimeout(fn, 0);
}
