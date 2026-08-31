import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { mkdir, readFile, rm } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_MODULE_PATH || 'playwright');
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sourceDir = path.join(root, 'src', 'icons', 'providers', 'source');
const outputDir = path.join(root, 'src', 'icons', 'providers', 'rendered', 'tiles');
const rawDir = path.join(root, 'tmp', 'provider-tile-supersample');
const downsampleScript = path.join(root, 'tools', 'downsample_provider_tile.py');
const supersample = 8;

// Each compact surface gets a native logical-size class. Every class is
// rendered independently at each standard Windows DPI bucket, so normal scale
// steps stay pixel-exact. Uncommon custom DPIs use only a small final resize
// from the nearest bucket.
const sizeClasses = [
  {
    suffix: '',
    buckets: [
      { dpi: 96, chip: 28, radius: 7, inset: 1, logo: 19 },
      { dpi: 120, chip: 35, radius: 9, inset: 1, logo: 24 },
      { dpi: 144, chip: 42, radius: 11, inset: 2, logo: 29 },
      { dpi: 168, chip: 49, radius: 12, inset: 2, logo: 33 },
      { dpi: 192, chip: 56, radius: 14, inset: 2, logo: 38 },
      { dpi: 216, chip: 63, radius: 16, inset: 2, logo: 43 },
      { dpi: 240, chip: 70, radius: 18, inset: 3, logo: 48 },
      { dpi: 288, chip: 84, radius: 21, inset: 3, logo: 57 },
      { dpi: 336, chip: 98, radius: 25, inset: 4, logo: 67 },
      { dpi: 384, chip: 112, radius: 28, inset: 4, logo: 76 },
    ],
  },
  {
    suffix: '-c20',
    buckets: [
      { dpi: 96, chip: 20, radius: 5, inset: 1, logo: 14 },
      { dpi: 120, chip: 25, radius: 6, inset: 1, logo: 18 },
      { dpi: 144, chip: 30, radius: 8, inset: 2, logo: 21 },
      { dpi: 168, chip: 35, radius: 9, inset: 2, logo: 25 },
      { dpi: 192, chip: 40, radius: 10, inset: 2, logo: 28 },
      { dpi: 216, chip: 45, radius: 11, inset: 2, logo: 32 },
      { dpi: 240, chip: 50, radius: 13, inset: 3, logo: 35 },
      { dpi: 288, chip: 60, radius: 15, inset: 3, logo: 42 },
      { dpi: 336, chip: 70, radius: 18, inset: 4, logo: 49 },
      { dpi: 384, chip: 80, radius: 20, inset: 4, logo: 56 },
    ],
  },
  {
    suffix: '-c16',
    buckets: [
      { dpi: 96, chip: 16, radius: 4, inset: 1, logo: 11 },
      { dpi: 120, chip: 20, radius: 5, inset: 1, logo: 14 },
      { dpi: 144, chip: 24, radius: 6, inset: 2, logo: 17 },
      { dpi: 168, chip: 28, radius: 7, inset: 2, logo: 19 },
      { dpi: 192, chip: 32, radius: 8, inset: 2, logo: 22 },
      { dpi: 216, chip: 36, radius: 9, inset: 2, logo: 25 },
      { dpi: 240, chip: 40, radius: 10, inset: 3, logo: 28 },
      { dpi: 288, chip: 48, radius: 12, inset: 3, logo: 33 },
      { dpi: 336, chip: 56, radius: 14, inset: 4, logo: 39 },
      { dpi: 384, chip: 64, radius: 16, inset: 4, logo: 44 },
    ],
  },
];
const viewportSize =
  Math.max(...sizeClasses.flatMap(({ buckets }) => buckets.map(({ chip }) => chip))) *
    supersample +
  32;

// Render the complete provider tile offline. This keeps the SVG mark and the
// rounded tile on one antialiasing grid instead of combining a PNG with a
// binary GDI rounded region at runtime.
const variants = [
  {
    name: 'claude-dark',
    source: 'claude.svg',
    background: '#30211E',
    border: '#70483D',
  },
  {
    name: 'claude-light',
    source: 'claude.svg',
    background: '#FFF0EA',
    border: '#F1C8BA',
  },
  {
    name: 'openai',
    source: 'openai.svg',
    color: '#000000',
    background: '#F7F7F5',
    border: '#D4D4D0',
  },
  {
    name: 'antigravity-dark',
    source: 'antigravity.svg',
    background: '#172B4A',
    border: '#3C68A4',
  },
  {
    name: 'antigravity-light',
    source: 'antigravity.svg',
    background: '#E8F0FF',
    border: '#BFD3FF',
  },
  // xAI ships one look for the Grok mark - white on near-black - so unlike
  // Claude and Antigravity it gets a single tile that both themes share. The
  // near-black carries the same cool tint the other dark tiles use: a neutral
  // grey collapses every blend onto one channel, which leaves a 20 px tile
  // below the generator's own antialiasing floor.
  {
    name: 'grok',
    source: 'grok.svg',
    color: '#FFFFFF',
    background: '#17171C',
    border: '#41414D',
  },
];

const executablePath = process.env.CHROMIUM_PATH;
if (!executablePath) throw new Error('CHROMIUM_PATH is required');
const pythonPath = process.env.PYTHON_PATH || (process.platform === 'win32' ? 'python' : 'python3');

await rm(rawDir, { recursive: true, force: true });
await mkdir(rawDir, { recursive: true });
await mkdir(outputDir, { recursive: true });

const browser = await chromium.launch({ executablePath, headless: true });
const context = await browser.newContext({
  colorScheme: 'dark',
  deviceScaleFactor: 1,
  // Derive this from the largest 8x tile so new DPI buckets cannot silently
  // outgrow the page and get clipped by locator screenshots.
  viewport: { width: viewportSize, height: viewportSize },
});
const page = await context.newPage();

try {
  for (const variant of variants) {
    let svg = await readFile(path.join(sourceDir, variant.source), 'utf8');
    svg = svg.replace('width="1em"', 'width="100%"');
    svg = svg.replace('height="1em"', 'height="100%"');

    for (const sizeClass of sizeClasses) {
      for (const bucket of sizeClass.buckets) {
        const rawChip = bucket.chip * supersample;
        const rawRadius = bucket.radius * supersample;
        const rawInset = bucket.inset * supersample;
        const rawInnerRadius = (bucket.radius - bucket.inset) * supersample;
        const rawLogo = bucket.logo * supersample;
        const rawOutput = path.join(
          rawDir,
          `${variant.name}${sizeClass.suffix}-${bucket.dpi}.png`,
        );
        const output = path.join(
          outputDir,
          `${variant.name}${sizeClass.suffix}-${bucket.dpi}.png`,
        );

        await page.setContent(`
        <style>
          html, body { margin: 0; background: transparent; overflow: hidden; }
          #tile {
            position: relative;
            width: ${rawChip}px;
            height: ${rawChip}px;
            overflow: hidden;
            border-radius: ${rawRadius}px;
            background: ${variant.border};
          }
          #surface {
            position: absolute;
            inset: ${rawInset}px;
            border-radius: ${rawInnerRadius}px;
            background: ${variant.background};
          }
          #logo {
            position: absolute;
            left: 50%;
            top: 50%;
            width: ${rawLogo}px;
            height: ${rawLogo}px;
            color: ${variant.color || '#000000'};
            transform: translate(-50%, -50%);
          }
        </style>
        <div id="tile"><div id="surface"></div><div id="logo">${svg}</div></div>
        `);

        const metadata = await page.evaluate(
          ({ expectedChip, expectedLogo }) => {
            const tile = document.querySelector('#tile').getBoundingClientRect();
            const logo = document.querySelector('#logo svg').getBoundingClientRect();
            return {
              tile: [tile.width, tile.height],
              logo: [logo.width, logo.height],
              expectedChip,
              expectedLogo,
            };
          },
          { expectedChip: rawChip, expectedLogo: rawLogo },
        );
        if (
          metadata.tile[0] !== rawChip ||
          metadata.tile[1] !== rawChip ||
          metadata.logo[0] !== rawLogo ||
          metadata.logo[1] !== rawLogo
        ) {
          throw new Error(
            `invalid tile geometry ${rawOutput}: ${JSON.stringify(metadata)}`,
          );
        }

        await page.locator('#tile').screenshot({
          animations: 'disabled',
          omitBackground: true,
          path: rawOutput,
          scale: 'css',
        });

        const downsample = spawnSync(
          pythonPath,
          [downsampleScript, rawOutput, output, String(bucket.chip), String(supersample)],
          { encoding: 'utf8' },
        );
        if (downsample.status !== 0) {
          throw new Error(
            `tile downsample failed for ${output}: ${downsample.stderr || downsample.stdout}`,
          );
        }
        console.log(
          `${path.relative(root, output)} ${bucket.chip}x${bucket.chip} ` +
            `(logo ${bucket.logo}px, ${supersample}x supersample)`,
        );
      }
    }
  }
} finally {
  await browser.close();
  await rm(rawDir, { recursive: true, force: true });
}
