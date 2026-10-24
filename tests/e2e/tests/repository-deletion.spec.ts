import { test, expect, APIRequestContext } from '@playwright/test';

async function publishNpm(request: APIRequestContext, name: string, version: string) {
  const filename = `${name}-${version}.tgz`;
  const response = await request.put(`/npm-local/${name}`, {
    data: {
      name,
      versions: { [version]: { name, version, dist: {} } },
      'dist-tags': { latest: version },
      _attachments: { [filename]: { data: Buffer.from('fixture').toString('base64') } },
    },
  });
  expect(response.status()).toBe(201);
}

async function uploadPython(request: APIRequestContext, name: string, version: string, filename: string) {
  const response = await request.post('/simple-local/', {
    multipart: {
      ':action': 'file_upload', name, version,
      content: { name: filename, mimeType: 'application/octet-stream', buffer: Buffer.from('fixture') },
    },
  });
  expect(response.ok()).toBeTruthy();
}

test('cancel, version deletion, final-package redirect and replacement upload', async ({ page, request }) => {
  const name = `delete-ui-npm-${Date.now()}`;
  await publishNpm(request, name, '1.0.0');
  await publishNpm(request, name, '2.0.0');
  await page.goto(`/ui/repositories/smoke-npm-local/package/${name}`);
  const latest = page.getByRole('row').filter({ has: page.getByRole('cell', { name: '2.0.0', exact: true }) });
  page.once('dialog', dialog => dialog.dismiss());
  await latest.getByRole('button', { name: 'Delete version', exact: true }).click();
  await expect(latest).toBeVisible();
  page.once('dialog', async dialog => {
    expect(dialog.message()).toContain('smoke-npm-local');
    await dialog.accept();
  });
  await latest.getByRole('button', { name: 'Delete version', exact: true }).click();
  await expect(latest).toHaveCount(0);
  const metadata = await (await request.get(`/npm-local/${name}`)).json();
  expect(metadata['dist-tags'].latest).toBe('1.0.0');
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Delete version', exact: true }).click();
  await expect(page).toHaveURL(/\/ui\/repositories\/smoke-npm-local$/);
  await publishNpm(request, name, '1.0.0');
  await page.goto(`/ui/repositories/smoke-npm-local/package/${name}`);
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Delete package', exact: true }).click();
  await expect(page).toHaveURL(/\/ui\/repositories\/smoke-npm-local$/);
});

test('PyPI groups all release files and stale package confirmation deletes current contents', async ({ page, request }) => {
  const name = `delete-ui-python-${Date.now()}`;
  const wheelName = name.replaceAll('-', '_');
  await uploadPython(request, name, '1.0', `${name}-1.0.zip`);
  await uploadPython(request, name, '1.0', `${wheelName}-1.0-py3-none-any.whl`);
  await page.goto(`/ui/repositories/smoke-python-local/package/${name}`);
  const release = page.getByRole('row').filter({ has: page.getByRole('cell', { name: '1.0', exact: true }) });
  await expect(release.getByRole('cell', { name: '2', exact: true })).toBeVisible();
  // Publication after page render is included, without snapshot conflicts.
  await uploadPython(request, name, '2.0', `${name}-2.0.zip`);
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Delete package', exact: true }).click();
  await expect(page).toHaveURL(/\/ui\/repositories\/smoke-python-local$/);
  expect((await request.get(`/simple-local/${name}/`)).status()).toBe(404);
});

test('failed non-JSON deletion retains exact retry across a refreshed empty view', async ({ page, request }) => {
  const name = `delete-ui-retry-${Date.now()}`;
  await uploadPython(request, name, '1.0', `${name}-1.0.zip`);
  await page.goto(`/ui/repositories/smoke-python-local/package/${name}`);
  await page.route('**/api/v1/repositories/**', route => route.fulfill({ status: 502, contentType: 'text/plain', body: 'gateway failure' }));
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Delete version', exact: true }).click();
  await expect(page.locator('#delete-error')).toContainText('Some files may already be gone');
  await expect(page).toHaveURL(/retry_version=1.0/);
  await page.unroute('**/api/v1/repositories/**');
  // Simulate an operation which completed but whose response was lost.
  expect((await request.delete(`/api/v1/repositories/smoke-python-local/packages/${name}?version=1.0`, { headers: { 'HX-Request': 'true' } })).ok()).toBeTruthy();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Retry version deletion', exact: true })).toBeVisible();
  page.once('dialog', dialog => dialog.accept());
  await page.getByRole('button', { name: 'Retry version deletion', exact: true }).click();
  await expect(page).toHaveURL(/\/ui\/repositories\/smoke-python-local$/);
});
