/**
 * Structural types for the slice of `@remotion/renderer` that runner-core
 * uses. runner-core deliberately has NO dependency on `@remotion/renderer`:
 * each versioned runner app imports its own pinned renderer and injects it
 * via {@link RendererApi}. Keeping the import in the
 * app directory is what guarantees `bun build --compile` embeds that app's
 * pinned renderer version — a direct import from this package would resolve
 * against this package's directory instead (where no renderer is installed,
 * and where hoisting could otherwise pick an arbitrary version).
 */

/**
 * The subset of a Remotion composition that runner-core reads.
 *
 * `width`/`height` are optional so this stays backward compatible for any
 * caller already implementing the type; a real Remotion composition always
 * carries them, so the production path gets the dimension check and only
 * hand-built test doubles skip it (which `verifyRenderedOutput` logs).
 */
export type MinimalComposition = {
  durationInFrames: number;
  width?: number;
  height?: number;
};

type SharedRenderOptions = {
  serveUrl: string;
  inputProps: Record<string, unknown>;
  binariesDirectory: string | null;
  /**
   * Absolute path to the browser shipped inside the render payload. When null,
   * Remotion resolves its own cache by walking up from `process.cwd()` for a
   * `package.json` — which, for a compiled runner spawned in a per-job workdir,
   * lands the download INSIDE that workdir and loses it to the purge on every
   * job. Always pass this in production.
   */
  browserExecutable: string | null;
  chromeMode: 'chrome-for-testing';
  chromiumOptions: {gl: 'angle'};
};

export type RendererApi<TComposition extends MinimalComposition> = {
  selectComposition: (options: SharedRenderOptions & {id: string}) => Promise<TComposition>;
  renderMedia: (
    options: SharedRenderOptions & {
      composition: TComposition;
      codec: 'vp8' | 'h264';
      colorSpace: 'bt709';
      outputLocation: string;
      concurrency: number;
      onProgress: (progress: {progress: number}) => void;
    },
  ) => Promise<unknown>;
};
