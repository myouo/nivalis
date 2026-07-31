# Nivalis Design QA

## Visual source

The original Stitch exports under `stitch_cloudpic_web_interface/` remain design-only source material. They are outside Vite's static asset directory and are not included in the production build.

## Current product state

- Personal single-user library protected by Cloudflare Access; there is no in-app account system.
- Upload, Gallery, Albums, Recycle Bin, and Settings are connected to real application state.
- Upload previews use local object URLs while files are queued and real delivery URLs after completion.
- The selected uploaded image controls the Grab Links panel.
- Settings persist the WebP quality and default share mode; every displayed control affects real behavior.
- Image details expose Original and Compressed WebP sharing for public and temporary signed links.
- Image details place the single share-mode selector directly above metadata save; temporary-link expiry, generation, and revocation remain one uninterrupted group.
- Upload exposes individual URL, Markdown, HTML, and BBCode copy controls without a redundant copy-all action.
- English, Simplified Chinese, Traditional Chinese, and Japanese are separated into independent dictionaries. Settings supports automatic browser detection and an explicit persisted language choice.
- Sidebar navigation uses real SPA URLs for gallery, upload, albums, recycle bin, and settings.
- The mobile Gallery view uses a compact two-column masonry layout without horizontal overflow.
- The application-level file input is mounted from startup. Quick Upload and the upload drop zone invoke it synchronously without a route transition, timer, or nested interactive control.
- Empty libraries, albums, and upload queues render honest empty states; no sample records or fallback gallery images are injected.

## Browser verification

- Public WebP delivery returned HTTP 200 with `Content-Type: image/webp`.
- Private signed WebP delivery returned HTTP 200 and rejected signature parameter tampering in automated tests.
- WebP output does not upscale a source smaller than the selected 1920px maximum.
- Browser-language checks passed for `en-US`, `zh-CN`, `zh-Hant-TW`, and `ja-JP`.
- Direct `/settings` requests receive the SPA shell; links, refresh, back, and forward navigation preserve the active route.
- An explicit Japanese selection survived a full refresh, then automatic language detection was restored.
- At a 390 × 844 mobile viewport, the gallery rendered two 173px columns and the document had no horizontal overflow.
- Browser console: no errors, warnings, or issues.

## Automated verification

- `npm run typecheck`: passed
- `npm test`: passed, 6 files and 27 tests
- `XDG_CONFIG_HOME=/tmp/nivalis-xdg npm run build`: passed

Final result: passed.
