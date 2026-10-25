// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

import { test, expect, APIRequestContext } from '@playwright/test';
import { createHash } from 'node:crypto';

const contents = Buffer.from('browser artifact inventory fixture');
const sha256 = createHash('sha256').update(contents).digest('hex');

async function uploadPython(request: APIRequestContext, name: string, version: string, filename: string) {
  const response = await request.post('/simple-local/', {
    multipart: {
      ':action': 'file_upload', name, version,
      content: { name: filename, mimeType: 'application/octet-stream', buffer: contents },
    },
  });
  expect(response.ok()).toBeTruthy();
}

test('PyPI version navigation, Other files, paged hashes and narrow-screen copying', async ({ page, request, context }) => {
  const name = `artifact-ui-python-${Date.now()}`;
  const version = '1.0+local';
  const wheel = `${name.replaceAll('-', '_')}-${version}-py3-none-any.whl`;
  const archive = `${name}-${version}.tar.gz`;
  const [first, second] = [wheel, archive].sort();
  await uploadPython(request, name, version, wheel);
  await uploadPython(request, name, version, archive);
  await uploadPython(request, name, '2.0', `${name}-2.0.tar.gz`);
  await uploadPython(request, name, version, 'orphan.whl');

  const overview = `/ui/repositories/smoke-python/package/${name}?limit=1`;
  await page.goto(overview);
  await expect(page.getByRole('heading', { name: 'Other files', exact: true })).toBeVisible();
  await expect(page.getByRole('cell', { name: 'orphan.whl', exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: 'View files for version 2.0', exact: true })).toBeVisible();
  await page.getByRole('link', { name: 'Next versions', exact: true }).click();
  await page.getByRole('link', { name: `View files for version ${version}`, exact: true }).click();
  await expect(page).toHaveURL(/version=1.0%2Blocal/);
  await expect(page.getByRole('cell', { name: first, exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Delete version', exact: true })).toHaveCount(0);
  await expect(page.getByRole('link', { name: 'smoke-python-local', exact: true })).toHaveAttribute('href', new RegExp(`version=1.0%2Blocal`));

  const row = page.getByRole('row').filter({ has: page.getByRole('cell', { name: first, exact: true }) });
  await expect(row.locator('code').first()).toHaveText(sha256);
  await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: new URL(page.url()).origin });
  await row.getByRole('button', { name: 'Copy sha256 checksum', exact: true }).first().click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(sha256);
  await expect(row.locator('code')).toHaveCount(1);
  await expect(row.locator('summary')).toHaveCount(0);
  await expect(row.getByRole('cell', { name: 'Stored', exact: true })).toBeVisible();
  await expect(row).not.toContainText('Storage pin');
  await expect(row).not.toContainText('Stored update');
  await expect(row).not.toContainText('File modified');

  await page.getByRole('link', { name: 'Next files', exact: true }).click();
  await expect(page.getByRole('cell', { name: second, exact: true })).toBeVisible();
  await expect(page.getByRole('cell', { name: first, exact: true })).toHaveCount(0);
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole('button', { name: 'Copy sha256 checksum', exact: true }).first().scrollIntoViewIfNeeded();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBeTruthy();
  await page.locator('#package-back').click();
  await expect(page.getByRole('heading', { name: 'Other files', exact: true })).toBeVisible();
});

test('scoped npm versions expose publisher hashes without adding file-view mutations', async ({ page, request }) => {
  const bare = `artifact-ui-npm-${Date.now()}`;
  const name = `@artifacts/${bare}`;
  const version = '1.0.0';
  const filename = `${bare}-${version}.tgz`;
  const integrity = `sha512-${createHash('sha512').update(contents).digest('base64')}`;
  const response = await request.put(`/npm-local/${encodeURIComponent(name)}`, {
    data: {
      name,
      versions: { [version]: { name, version, dist: { integrity, shasum: createHash('sha1').update(contents).digest('hex') } } },
      'dist-tags': { latest: version },
      _attachments: { [filename]: { data: contents.toString('base64') } },
    },
  });
  expect(response.status()).toBe(201);
  await page.goto(`/ui/repositories/smoke-npm-local/package/${encodeURIComponent(name)}`);
  await page.getByRole('link', { name: `View files for version ${version}`, exact: true }).click();
  await expect(page.getByRole('cell', { name: filename, exact: true })).toBeVisible();
  await expect(page.locator('main code').first()).toHaveText(sha256);
  await expect(page.getByRole('button', { name: 'Delete package', exact: true })).toHaveCount(0);
  const detail = await (await request.get(`/api/ui/repositories/smoke-npm-local/package/${encodeURIComponent(name)}?version=${version}`)).json();
  expect(detail.artifacts.files.items[0].checksums.some((hash: { value: string }) => hash.value === integrity)).toBeTruthy();
  await expect(page.locator('main table summary')).toHaveText('More checksums (2)');
  await page.locator('main table summary').click();
  await expect(page.locator('main table code')).toHaveCount(3);
  await expect(page.getByText(`Publisher metadata (${version})`, { exact: false })).toHaveCount(0);
  await expect(page.locator('main table')).not.toContainText('Storage pin');
});
