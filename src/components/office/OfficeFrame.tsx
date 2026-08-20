import { forwardRef, useCallback, useEffect, useImperativeHandle, useRef, useState } from "react";
import type { FrameSelState, HostMessage } from "./protocol";
import { isFrameMessage } from "./protocol";

type Slot = "a" | "b";
const SLOTS: readonly Slot[] = ["a", "b"];
const other = (s: Slot): Slot => (s === "a" ? "b" : "a");

export interface OfficeFrameHandle {
  /** Posts to the *front* buffer. No-op before the first `ready`. */
  send(msg: HostMessage): boolean;
  /** Painted box of the buffer on screen, for mapping frame coordinates into the
   *  host's (the selection popover is rendered host-side). */
  rect(): DOMRect | null;
}

interface Props {
  /** Complete document, from buildOfficeSrcdoc. */
  srcdoc: string;
  /** The token baked into `srcdoc`; identifies that document's messages. */
  token: string;
  title: string;
  /** Fires when a buffer has parsed and centred its match, i.e. when it is shown. */
  onReady?: () => void;
  /** In-frame selection state changed (or its anchor moved under a scroll/zoom). */
  onSel?: (state: FrameSelState) => void;
  /** The frame's zoom changed (ctrl+wheel in-frame, or an ack for `zoom`). */
  onZoomed?: (factor: number) => void;
  /** The frame took focus and wants the host to take it back. */
  onRefocus?: () => void;
}

/**
 * Double-buffered host for a rendered office document.
 *
 * Two iframes with *stable* keys ("a"/"b"). Setting `srcDoc` on a live iframe is
 * a navigation, and unlike a bitmap a document cannot be pre-decoded off-screen
 * and swapped in - so the only way to avoid a white flash on every content change
 * is to own two documents: navigate the **back** buffer, leave the front one
 * painting, and flip once the back buffer says `ready` with a matching token. The
 * frame that was showing keeps showing until the replacement is complete.
 *
 * Consequences worth not undoing:
 *
 *  - The back buffer is `visibility:hidden; pointer-events:none`, never
 *    `display:none`. `display:none` takes it out of layout, which lets WebKit
 *    defer its load - the exact work we are trying to do in advance - and gives
 *    it a zero-sized viewport, which the bootstrap measures its opening view
 *    against, so the best match would be centred in nothing.
 *  - Neither iframe is ever keyed by path or content. A changed key remounts the
 *    element, which destroys the painted document, which is the flash.
 */
const OfficeFrame = forwardRef<OfficeFrameHandle, Props>(function OfficeFrame(
  { srcdoc, token, title, onReady, onSel, onZoomed, onRefocus },
  ref,
) {
  const [docs, setDocs] = useState<Record<Slot, string | null>>({ a: null, b: null });
  const [front, setFront] = useState<Slot>("a");

  const frames = useRef<Record<Slot, HTMLIFrameElement | null>>({ a: null, b: null });
  // Which buffer is loading which document, and which document is on screen.
  const pending = useRef<{ slot: Slot; token: string } | null>(null);
  const live = useRef<{ slot: Slot; token: string } | null>(null);

  // Callbacks through refs: the message listener is installed once, and
  // re-installing it on every parent render would drop messages in the gap.
  const onReadyRef = useRef(onReady);
  onReadyRef.current = onReady;
  const onSelRef = useRef(onSel);
  onSelRef.current = onSel;
  const onZoomedRef = useRef(onZoomed);
  onZoomedRef.current = onZoomed;
  const onRefocusRef = useRef(onRefocus);
  onRefocusRef.current = onRefocus;

  // Navigate whichever buffer is free. The first document goes into the front
  // buffer (there is nothing to hold on screen), every later one into the back.
  useEffect(() => {
    if (live.current?.token === token || pending.current?.token === token) return;
    const slot = live.current === null ? front : other(live.current.slot);
    pending.current = { slot, token };
    setDocs(prev => ({ ...prev, [slot]: srcdoc }));
    // `front` is intentionally not a dependency: it only ever changes as a
    // *result* of this effect's navigation completing.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [srcdoc, token]);

  useEffect(() => {
    const onMessage = (e: MessageEvent) => {
      // e.origin is "null" for an opaque origin and proves nothing; the sender
      // window plus the per-document token is what identifies a frame.
      const slot = SLOTS.find(s => frames.current[s]?.contentWindow === e.source);
      if (!slot || !isFrameMessage(e.data)) return;
      const msg = e.data;
      if (msg.type === "ready") {
        const p = pending.current;
        if (!p || p.slot !== slot || p.token !== msg.token) return;
        pending.current = null;
        live.current = { slot, token: msg.token };
        setFront(slot);
        onReadyRef.current?.();
        return;
      }
      // Focus is honoured from either buffer. A back buffer is pointer-events:none
      // so it should never get focus, but if it does the keybinds are just as dead
      // as they would be from the front one.
      if (msg.type === "refocus") {
        onRefocusRef.current?.();
        return;
      }
      // Everything else only counts from the document actually on screen: a back
      // buffer still settling must not report its selection as the user's.
      if (live.current?.token !== msg.token) return;
      if (msg.type === "sel") onSelRef.current?.(msg);
      else if (msg.type === "zoomed") onZoomedRef.current?.(msg.factor);
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, []);

  const send = useCallback((msg: HostMessage): boolean => {
    const l = live.current;
    const win = l && frames.current[l.slot]?.contentWindow;
    if (!l || !win) return false;
    // targetOrigin "*": the frame's origin is opaque, so it cannot be named. The
    // token in the envelope is what the frame authenticates on.
    win.postMessage({ ...msg, token: l.token }, "*");
    return true;
  }, []);

  const rect = useCallback((): DOMRect | null => {
    const l = live.current;
    return (l && frames.current[l.slot]?.getBoundingClientRect()) ?? null;
  }, []);

  useImperativeHandle(ref, () => ({ send, rect }), [send, rect]);

  return (
    <>
      {SLOTS.map(slot =>
        docs[slot] === null ? null : (
          <iframe
            key={slot}
            ref={el => { frames.current[slot] = el; }}
            className={`office-frame${slot === front ? "" : " is-back"}`}
            // allow-scripts *without* allow-same-origin - see buildOfficeSrcdoc
            // for why that pair must never appear together.
            sandbox="allow-scripts"
            srcDoc={docs[slot]!}
            title={title}
          />
        ),
      )}
    </>
  );
});

export default OfficeFrame;
