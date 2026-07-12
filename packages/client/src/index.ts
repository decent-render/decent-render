import {createHash, createHmac, timingSafeEqual} from 'node:crypto';
import {createTarGzip} from './archive.js';
import {
  apiErrorSchema,
  balanceResponseSchema,
  bundleCompleteResponseSchema,
  bundleUploadResponseSchema,
  cancelRenderResponseSchema,
  enqueueRenderRequestSchema,
  enqueueRenderResponseSchema,
  renderStatusResponseSchema,
  webhookCreateResponseSchema,
  webhookDeleteResponseSchema,
  webhookEndpointSchema,
  webhookListResponseSchema,
  versionsResponseSchema,
  type BalanceResponse,
  type CancelRenderResponse,
  type EnqueueRenderRequest,
  type RenderStatusResponse,
  type WebhookEndpoint,
  type VersionsResponse,
} from './schemas.js';

export * from './schemas.js';

const DEFAULT_API_URL = 'https://decent-render-dispatch.fly.dev';

type Auth = {apiKey: string; apiUrl?: string};
type RequestOptions = Auth & {signal?: AbortSignal};

export class FarmApiError extends Error {
  constructor(public readonly status: number, message: string, public readonly code?: string, public readonly details?: unknown) {
    super(message);
    this.name = 'FarmApiError';
  }
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

export function getVersions(options: RequestOptions): Promise<VersionsResponse> {
  return requestJson(options, '/api/v1/versions', versionsResponseSchema);
}

export type RenderMediaOnFarmOptions = RequestOptions & EnqueueRenderRequest & {
  pollIntervalMs?: number;
  timeoutMs?: number;
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

export async function renderMediaOnFarm(options: RenderMediaOnFarmOptions): Promise<RenderMediaOnFarmResult> {
  const enqueued = await enqueueRender(options);
  const deadline = Date.now() + (options.timeoutMs ?? 30 * 60 * 1000);
  for (;;) {
    const status = options.waitForCompletion
      ? await options.waitForCompletion(enqueued.renderId)
      : await getRenderProgress({...options, renderId: enqueued.renderId});
    if (status.status === 'complete') {
      return {outputUrl: status.outputUrl, renderId: status.renderId, creditsSettled: status.creditsSettled, verification: status.verification};
    }
    if (status.status === 'failed' || status.status === 'canceled') {
      throw new FarmApiError(409, status.error ?? `Render ${status.status}`, `RENDER_${status.status.toUpperCase()}`, status);
    }
    if (options.waitForCompletion) throw new FarmApiError(500, 'Webhook completion callback returned a non-terminal status');
    if (Date.now() >= deadline) throw new FarmApiError(408, `Timed out waiting for render ${enqueued.renderId}`, 'RENDER_TIMEOUT');
    await sleep(options.pollIntervalMs ?? 1000, options.signal);
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
    if (!upload.uploadUrl) throw new FarmApiError(500, 'Farm did not provide a bundle upload URL');
    const uploaded = await fetch(upload.uploadUrl, {method: 'PUT', body: Uint8Array.from(archive), signal: options.signal});
    if (!uploaded.ok) throw new FarmApiError(uploaded.status, `Bundle upload failed (${uploaded.status})`);
    const completed = await requestJson(options, `/api/v1/bundles/${sha256}/complete`, bundleCompleteResponseSchema, {
      method: 'POST', body: JSON.stringify({remotionVersion: options.remotionVersion, sizeBytes: archive.byteLength}),
    });
    if (completed.sha256 !== sha256) throw new FarmApiError(500, 'Farm registered a different bundle SHA-256');
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
