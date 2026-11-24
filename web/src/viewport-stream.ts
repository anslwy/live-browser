const MAX_FRAME_BYTES = 16 * 1024 * 1024;

/** Read big-endian length-prefixed JPEGs. A zero-length packet is a heartbeat. */
export async function readViewportFrames(
  body: ReadableStream<Uint8Array>,
  onFrame: (frame: Uint8Array<ArrayBuffer>) => void,
  onActivity: () => void,
) {
  const reader = body.getReader();
  const length = new Uint8Array(4);
  let frame: Uint8Array<ArrayBuffer> | null = null;
  let used = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) return;
      onActivity();
      let offset = 0;
      while (offset < value.length) {
        const target = frame ?? length;
        const count = Math.min(target.length - used, value.length - offset);
        target.set(value.subarray(offset, offset + count), used);
        used += count;
        offset += count;
        if (used !== target.length) continue;
        used = 0;
        if (frame) {
          onFrame(frame);
          frame = null;
        } else {
          const size = new DataView(length.buffer).getUint32(0);
          if (size > MAX_FRAME_BYTES) throw new Error('Invalid viewport frame length');
          if (size !== 0) frame = new Uint8Array(size);
        }
      }
    }
  } finally {
    await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}
