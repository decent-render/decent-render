import {createHash, createHmac, timingSafeEqual} from 'node:crypto';
import {createTarGzip} from './archive.js';
import {
  apiErrorSchema,
  balanceResponseSchema,
  bundleCompleteResponseSchema,
  bundleUploadResponseSchema,
  latestBundleResponseSchema,
  cancelRenderResponseSchema,
  enqueueRenderRequestSchema,
  enqueueRenderResponseSchema,
  renderStatusResponseSchema,
  webhookCreateResponseSchema,
  webhookDeleteResponseSchema,
  webhookEndpointSchema,
  webhookListResponseSchema,
  versionsResponseSchema,
  workerAvailabilityResponseSchema,
  type BalanceResponse,
  type LatestBundleResponse,
  type CancelRenderResponse,
  type EnqueueRenderRequest,
  type RenderStatusResponse,
  type WebhookEndpoint,
  type VersionsResponse,
  type WorkerAvailabilityResponse,
} from './schemas.js';

// D-15 (U-16): the zod schemas live behind the `@decent-render/client/schemas`
// subpath (package.json `exports`). The root re-exports ONLY the types the
// public function signatures above need — a bare `export *` here would leak
// the whole schema module through the root entry point (guarded against by
// src/__tests__/surface.test.ts).
export type {
  BalanceResponse,
  CancelRenderResponse,
  EnqueueRenderRequest,
  EnqueueRenderResponse,
  LatestBundleResponse,
  RenderStatusResponse,
  VersionsResponse,
  WebhookEndpoint,
  WorkerAvailabilityResponse,
} from './schemas.js';

const DEFAULT_API_URL = 'https://decent-render-dispatch.fly.dev';

type Auth = {apiKey: string; apiUrl?: string};
type RequestOptions = Auth & {signal?: AbortSignal};

/**
 * An error thrown by this package. `kind` tells you WHERE it failed:
 *
 * - `kind: 'http'` — `status` is a REAL HTTP status code from a farm or
 *   storage response; `code`/`details` carry the farm's error body.
 * - `kind: 'client'` — the failure happened OFF the HTTP path (a poll
 *   timeout, an abort, a terminal render state delivered inside a 200
 *   poll response, a client-side precondition). `status` is SYNTHETIC
 *   here: an HTTP-shaped hint (408 timeout, 409 conflict, 500 client
 *   bug) that no server sent — never treat it as a response status.
 */
export class FarmApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
    public readonly code?: string,
    public readonly details?: unknown,
    public readonly kind: 'http' | 'client' = 'http',
  ) {
    super(message);
    this.name = 'FarmApiError';
  }
}

/** Type guard: true when `error` is a {@link FarmApiError} from this package. */
export function isFarmApiError(error: unknown): error is FarmApiError {
  return error instanceof FarmApiError;
}

function endpoint(options: Auth, pathname: string): string {
  return `${(options.apiUrl ?? DEFAULT_API_URL).replace(/\/$/, '')}${pathname}`;
}

async function requestJson<T>(options: RequestOptions, pathname: string, schema: {parse(value: unknown): T}, init?: RequestInit): Promise<T> {
  const response = await fetch(endpoint(options, pathname), {
    ...init,
    signal: options.signal,
    headers: {
      authorization: `Bearer ${options.apiKey}`,
      ...(init?.body ? {'content-type': 'application/json'} : {}),
      ...init?.headers,
    },
  });
  const body = await response.json().catch(() => null);
  if (!response.ok) {
    const parsed = apiErrorSchema.safeParse(body);
    throw new FarmApiError(response.status, parsed.success ? parsed.data.error : `Farm API request failed (${response.status})`, parsed.success ? parsed.data.code : undefined, body);
  }
  return schema.parse(body);
}

export type GetRenderProgressOptions = RequestOptions & {renderId: string};
export function getRenderProgress(options: GetRenderProgressOptions): Promise<RenderStatusResponse> {
  return requestJson(options, `/api/v1/renders/${encodeURIComponent(options.renderId)}`, renderStatusResponseSchema);
}

export type CancelRenderOptions = RequestOptions & {renderId: string};
export function cancelRender(options: CancelRenderOptions): Promise<CancelRenderResponse> {
  return requestJson(options, `/api/v1/renders/${encodeURIComponent(options.renderId)}/cancel`, cancelRenderResponseSchema, {method: 'POST'});
}

export function getBalance(options: RequestOptions): Promise<BalanceResponse> {
  return requestJson(options, '/api/v1/balance', balanceResponseSchema);
}

export function getLatestBundle(options: RequestOptions): Promise<LatestBundleResponse> {
  return requestJson(options, '/api/v1/bundles/latest', latestBundleResponseSchema);
}

export function getWorkerAvailability(
  options: RequestOptions & {operator: string},
): Promise<WorkerAvailabilityResponse> {
  return requestJson(
    options,
    `/api/v1/workers/availability?operator=${encodeURIComponent(options.operator)}`,
    workerAvailabilityResponseSchema,
  );
}

export function getVersions(options: RequestOptions): Promise<VersionsResponse> {
  return requestJson(options, '/api/v1/versions', versionsResponseSchema);
}

export type RenderMediaOnFarmOptions = RequestOptions & EnqueueRenderRequest & {
  pollIntervalMs?: number;
  timeoutMs?: number;
  onProgress?: (status: RenderStatusResponse) => void;
  waitForCompletion?: (renderId: string) => Promise<RenderStatusResponse>;
};
export type RenderMediaOnFarmResult = {outputUrl: string; renderId: string; creditsSettled: number; verification: RenderStatusResponse['verification']};

export type EnqueueRenderOptions = RequestOptions & EnqueueRenderRequest;
export function enqueueRender(options: EnqueueRenderOptions) {
  const renderRequest = enqueueRenderRequestSchema.parse(options);
  return requestJson(options, '/api/v1/renders', enqueueRenderResponseSchema, {
    method: 'POST', body: JSON.stringify(renderRequest),
  });
}

const sleep = (ms: number, signal?: AbortSignal) => new Promise<void>((resolve, reject) => {
  if (signal?.aborted) return reject(signal.reason);
  const timer = setTimeout(resolve, ms);
  signal?.addEventListener('abort', () => { clearTimeout(timer); reject(signal.reason); }, {once: true});
});

/**
 * Enqueue a render and wait for it to finish.
 *
 * **Cancels what it abandons.** If this call stops waiting — the internal
 * `timeoutMs` (default 30 min) elapses, or the caller's `signal` aborts — it
 * POSTs the render's cancel endpoint BEFORE throwing, so the farm does not
 * keep rendering (and billing) a job nobody is waiting for. The cancel is
 * best-effort: a failure to cancel is swallowed and the original timeout /
 * abort error is what you get. It is sent without the aborted `signal`, so an
 * abort cannot cancel the cancel. Renders that reach `complete`, `failed`, or
 * `canceled` on their own are never canceled by this function.
 *
 * Other errors (a network failure, a farm 5xx while polling) do NOT cancel:
 * the render may still finish, and the farm will settle it; use
 * `getRenderProgress()` / `cancelRender()` with the `renderId` if you want to
 * resume or stop it.
 */
export async function renderMediaOnFarm(options: RenderMediaOnFarmOptions): Promise<RenderMediaOnFarmResult> {
  const enqueued = await enqueueRender(options);
  const deadline = Date.now() + (options.timeoutMs ?? 30 * 60 * 1000);

  /** Best-effort cancel of the render we are walking away from, then rethrow. */
  const abandon = async (error: unknown): Promise<never> => {
    // No `signal`: on the abort path it is already aborted and would abort
    // the cancel request itself before it reached the farm.
    await cancelRender({apiKey: options.apiKey, apiUrl: options.apiUrl, renderId: enqueued.renderId}).catch(() => undefined);
    throw error;
  };

  for (;;) {
    let status: RenderStatusResponse;
    try {
      status = options.waitForCompletion
        ? await options.waitForCompletion(enqueued.renderId)
        : await getRenderProgress({...options, renderId: enqueued.renderId});
    } catch (error) {
      // An abort during the poll request surfaces as the fetch rejecting.
      if (options.signal?.aborted) return abandon(error);
      throw error;
    }
    options.onProgress?.(status);
    if (status.status === 'complete') {
      return {outputUrl: status.outputUrl, renderId: status.renderId, creditsSettled: status.creditsSettled, verification: status.verification};
    }
    if (status.status === 'failed' || status.status === 'canceled') {
      throw new FarmApiError(409, status.error ?? `Render ${status.status}`, `RENDER_${status.status.toUpperCase()}`, status, 'client');
    }
    if (options.waitForCompletion) throw new FarmApiError(500, 'Webhook completion callback returned a non-terminal status', undefined, undefined, 'client');
    if (Date.now() >= deadline) {
      return abandon(new FarmApiError(408, `Timed out waiting for render ${enqueued.renderId}`, 'RENDER_TIMEOUT', undefined, 'client'));
    }
    try {
      await sleep(options.pollIntervalMs ?? 1000, options.signal);
    } catch (error) {
      // An abort during the poll interval rejects the sleep with the reason.
      return abandon(error);
    }
  }
}

export type BundleAndUploadOptions = RequestOptions & {
  entryPoint: string;
  remotionVersion: string;
  webpackOverride?: (config: unknown) => unknown;
  onProgress?: (progress: number) => void;
};
export type BundleAndUploadResult = {sha256: string; remotionVersion: string; sizeBytes: number; alreadyRegistered: boolean};

export async function bundleAndUpload(options: BundleAndUploadOptions): Promise<BundleAndUploadResult> {
  const {bundle} = await import('@remotion/bundler');
  const bundleLocation = await bundle({
    entryPoint: options.entryPoint,
    webpackOverride: options.webpackOverride as never,
    onProgress: options.onProgress,
  });
  const archive = await createTarGzip(bundleLocation);
  const sha256 = createHash('sha256').update(archive).digest('hex');
  const metadata = {sha256, remotionVersion: options.remotionVersion, sizeBytes: archive.byteLength};
  const upload = await requestJson(options, '/api/v1/bundles', bundleUploadResponseSchema, {
    method: 'POST', body: JSON.stringify(metadata),
  });
  if (!upload.alreadyRegistered) {
    if (!upload.uploadUrl) throw new FarmApiError(500, 'Farm did not provide a bundle upload URL', undefined, undefined, 'client');
    const uploaded = await fetch(upload.uploadUrl, {method: 'PUT', body: Uint8Array.from(archive), signal: options.signal});
    if (!uploaded.ok) throw new FarmApiError(uploaded.status, `Bundle upload failed (${uploaded.status})`);
    const completed = await requestJson(options, `/api/v1/bundles/${sha256}/complete`, bundleCompleteResponseSchema, {
      method: 'POST', body: JSON.stringify({remotionVersion: options.remotionVersion, sizeBytes: archive.byteLength}),
    });
    if (completed.sha256 !== sha256) throw new FarmApiError(500, 'Farm registered a different bundle SHA-256', undefined, undefined, 'client');
  }
  return {...metadata, alreadyRegistered: upload.alreadyRegistered};
}

export function verifyWebhookSignature(options: {body: string | Uint8Array; timestamp: string; signature: string; secret: string}): boolean {
  const body = typeof options.body === 'string' ? options.body : Buffer.from(options.body).toString('utf8');
  const expected = createHmac('sha256', options.secret).update(`${options.timestamp}.${body}`).digest('hex');
  const left = Buffer.from(expected, 'hex');
  const right = Buffer.from(options.signature, 'hex');
  return left.length === right.length && timingSafeEqual(left, right);
}

export function listWebhooks(options: RequestOptions): Promise<{endpoints: WebhookEndpoint[]}> {
  return requestJson(options, '/api/v1/webhooks', webhookListResponseSchema);
}
export function createWebhook(options: RequestOptions & {url: string}) {
  return requestJson(options, '/api/v1/webhooks', webhookCreateResponseSchema, {method: 'POST', body: JSON.stringify({url: options.url})});
}
export function updateWebhook(options: RequestOptions & {webhookId: string; url?: string; isActive?: boolean}) {
  return requestJson(options, `/api/v1/webhooks/${encodeURIComponent(options.webhookId)}`, webhookEndpointSchema, {method: 'PATCH', body: JSON.stringify({url: options.url, isActive: options.isActive})});
}
export function deleteWebhook(options: RequestOptions & {webhookId: string}) {
  return requestJson(options, `/api/v1/webhooks/${encodeURIComponent(options.webhookId)}`, webhookDeleteResponseSchema, {method: 'DELETE'});
}
