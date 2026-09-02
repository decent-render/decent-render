# @decent-render/client

Type-safe Remotion farm client. It uses the public HTTP API only: no database or object-storage credentials.

```bash
bun add @decent-render/client @remotion/bundler
```

```ts
import {bundleAndUpload, renderMediaOnFarm} from '@decent-render/client';

const auth = {apiKey: process.env.DECENT_API_KEY!};
const uploaded = await bundleAndUpload({
  ...auth,
  entryPoint: './src/remotion/index.ts',
  remotionVersion: '4.0.487',
});
const result = await renderMediaOnFarm({
  ...auth,
  bundleSha256: uploaded.sha256,
  compositionId: 'Main',
  inputProps: {},
  compositionWidth: 1920,
  compositionHeight: 1080,
  fps: 30,
  durationFrames: 90,
  codec: 'h264',
});
console.log(result.outputUrl);
console.log(result.verification); // pending, passed, or flagged
```

Public functions:

- `bundleAndUpload()` — validates `remotionVersion` (strict semver,
  `major.minor.patch`) BEFORE archiving; a malformed version throws a
  `FarmApiError` with `kind: 'client'` and no fs/network work done.
- `renderMediaOnFarm()`
- `enqueueRender()` — enqueue a render and return immediately with its
  `renderId`; poll with `getRenderProgress()` and cancel with
  `cancelRender()`. `renderMediaOnFarm()` is this loop done for you.
- `getRenderProgress()`
- `cancelRender()`
- `getBalance()`
- `getLatestBundle()`
- `getWorkerAvailability()` — which workers behind an `operator` are
  currently available.
- `getVersions()`
- `verifyWebhookSignature()` — HMAC check **and** a replay window: deliveries whose
  `X-Decent-Timestamp` is more than `toleranceSeconds` (default 300) from now are
  rejected before the signature is compared. Dedupe retries on `X-Decent-Delivery-Id`.
- `listWebhooks()`, `createWebhook()`, `updateWebhook()`, `deleteWebhook()`

`renderMediaOnFarm()` cancels what it abandons: if its `timeoutMs` (default
30 minutes) elapses or the `signal` you pass aborts, it POSTs the render's
cancel endpoint before throwing, so the farm stops rendering (and billing) a
job nobody is waiting for. The cancel is best-effort and never masks the
original timeout/abort error. Renders that finish, fail, or get canceled on
their own are left alone; a network error while polling does not cancel
either, because the render may still complete — use `getRenderProgress()` or
`cancelRender()` with the `renderId` to resume or stop it yourself.

`getVersions()` returns the active farm-managed runner matrix. Unsupported
bundle registration or enqueue requests fail with
`UNSUPPORTED_REMOTION_VERSION` and the supported version names. Completed
renders always expose their output immediately; verification can later be
`passed` or `flagged` without removing the output URL. On complete statuses
`creditsSettled` is `number | null` — null when the job completed before
measured settlement existed (pre-migration-0016 rows), not a zero.

`@remotion/bundler` is an optional peer dependency used only by `bundleAndUpload()`. The package's only runtime dependency is Zod. Response types are inferred from the exported Zod schemas that the farm handlers also use.

Exported alongside the functions: the `FarmApiError` class — thrown for every
non-2xx API response, carrying the HTTP `status`, the farm's error `code`, and
the raw `details` body; the per-call option/result types (`GetRenderProgressOptions`,
`CancelRenderOptions`, `RenderMediaOnFarmOptions`, `RenderMediaOnFarmResult`,
`EnqueueRenderOptions`, `BundleAndUploadOptions`, `BundleAndUploadResult`);
and a re-export of the response/request TYPES the function signatures use.

**`FarmApiError.kind`** — `'http' | 'client'` tells you where an error came
from. `kind: 'http'` means `status` is a REAL HTTP status code from a farm or
storage response. `kind: 'client'` means the failure happened off the HTTP
path (poll timeout, abort, a terminal render state delivered inside a 200
poll, a client-side precondition like the `remotionVersion` check) and the
`status` is SYNTHETIC — an HTTP-shaped hint no server sent. Use the
`isFarmApiError(e)` type guard to distinguish these from other errors.

**`./schemas` subpath** — the Zod request/response schemas are importable
from `@decent-render/client/schemas`. The root entry point no longer
re-exports them: it exports only the response/request types the function
signatures use. If you imported schemas from the root, move those imports to
the subpath.
