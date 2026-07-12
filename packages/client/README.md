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

- `bundleAndUpload()`
- `renderMediaOnFarm()`
- `getRenderProgress()`
- `cancelRender()`
- `getBalance()`
- `getVersions()`
- `verifyWebhookSignature()`
- `listWebhooks()`, `createWebhook()`, `updateWebhook()`, `deleteWebhook()`

`getVersions()` returns the active farm-managed runner matrix. Unsupported
bundle registration or enqueue requests fail with
`UNSUPPORTED_REMOTION_VERSION` and the supported version names. Completed
renders always expose their output immediately; verification can later be
`passed` or `flagged` without removing the output URL.

`@remotion/bundler` is an optional peer dependency used only by `bundleAndUpload()`. The package's only runtime dependency is Zod. Response types are inferred from the exported Zod schemas that the farm handlers also use.
