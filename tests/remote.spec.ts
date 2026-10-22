import { expect, test, type Page } from '@playwright/test';

/* PandaCode Connect on the preview's sample account (src/preview-remote.ts): sign in, allow
 * remote control, see another computer appear, open one of its tasks and message it. */
async function openRemoteSettings(page: Page) {
  await page.goto('/');
  await page.locator('#navigation .task-item').first().waitFor();
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('tab', { name: 'Remote access' }).click();
  return page.getByRole('tabpanel', { name: 'Remote access' });
}

test('signs in with a device code, toggles remote control and lists devices', async ({ page }) => {
  const panel = await openRemoteSettings(page);
  await expect(page.locator('.sidebar-computers')).toBeHidden();
  await panel.getByRole('button', { name: 'Sign in with GitHub' }).click();
  await expect(panel.getByLabel('Device code')).toHaveText('WDJB-MJHT');
  await expect(panel.getByRole('button', { name: 'Copy code' })).toBeVisible();
  await expect(panel.getByText('octocat')).toBeVisible();
  await expect(panel.getByRole('button', { name: 'Sign out' })).toBeVisible();

  const toggle = panel.getByRole('checkbox', { name: 'Allow remote control of this computer' });
  await expect(toggle).not.toBeChecked();
  await toggle.check();
  await expect(toggle).toBeChecked();
  await expect(panel.locator('.remote-device[data-device="dev-this"]')).toContainText('This computer');
  await expect(panel.locator('.remote-device[data-device="dev-this"]')).toContainText('remote control on');

  const studio = panel.locator('.remote-device[data-device="dev-studio"]');
  await expect(studio).toContainText('Studio iMac');
  await expect(studio).toContainText('9b02e4aa71c3d605');
  await expect(studio.getByRole('img', { name: 'Online' })).toBeVisible();
  await expect(panel.locator('.remote-device[data-device="dev-laptop"]').getByRole('img', { name: 'Offline' })).toBeVisible();

  // Trust toggles show on other computers only.
  await expect(panel.locator('.remote-device[data-device="dev-this"] [data-trust]')).toHaveCount(0);
  const trust = panel.getByRole('checkbox', { name: 'Trusted: Studio iMac' });
  await expect(trust).toBeChecked();
  await trust.uncheck();
  await expect(trust).not.toBeChecked();
  await trust.check();
  await expect(trust).toBeChecked();
  await expect(panel.getByRole('checkbox', { name: 'Trusted: Home NAS' })).not.toBeChecked();
  await expect(panel.locator('.remote-trust .info-mark').first()).toHaveAttribute('data-tip', /compare the fingerprint/i);

  await panel.getByRole('button', { name: 'Revoke Old laptop' }).click();
  await expect(panel.locator('.remote-device[data-device="dev-laptop"]')).toHaveCount(0);
  await panel.getByRole('button', { name: 'Sign out' }).click();
  await expect(panel.getByRole('button', { name: 'Sign in with GitHub' })).toBeVisible();
});

test('a signed-in computer appears in the sidebar; open a task and send a message', async ({ page }) => {
  const panel = await openRemoteSettings(page);
  await panel.getByRole('button', { name: 'Sign in with GitHub' }).click();
  await expect(panel.getByRole('button', { name: 'Sign out' })).toBeVisible();
  await page.getByRole('button', { name: /^back/i }).click();

  const computer = page.locator('.sidebar-computers').getByRole('button', { name: 'Studio iMac' });
  await expect(computer).toBeVisible();
  await expect(page.locator('.sidebar-computers')).not.toContainText('Old laptop');
  await computer.click();

  const view = page.getByRole('region', { name: 'Remote computer' });
  await view.getByRole('button', { name: /Fix the flaky export test/ }).click();
  await expect(view.getByRole('log', { name: 'Conversation' })).toContainText('The test races the file watcher');
  await expect(view.locator('strong', { hasText: 'added a wait' })).toBeVisible();

  await view.getByRole('textbox', { name: 'Message' }).fill('Thanks, now run the whole suite');
  await view.getByRole('button', { name: 'Send' }).click();
  await expect(view.getByRole('log', { name: 'Conversation' })).toContainText('Thanks, now run the whole suite');
  await expect(view.getByRole('log', { name: 'Conversation' })).toContainText('Done: Thanks, now run the whole suite');

  await view.getByRole('button', { name: 'New task' }).click();
  await view.getByRole('textbox', { name: 'New task prompt' }).fill('Update the changelog');
  await view.getByRole('button', { name: 'Start task' }).click();
  await expect(view.getByRole('button', { name: /Update the changelog/ })).toBeVisible();
});

test('an untrusted computer shows a named empty state, and host text cannot inject markup', async ({ page }) => {
  const panel = await openRemoteSettings(page);
  await panel.getByRole('button', { name: 'Sign in with GitHub' }).click();
  await expect(panel.getByRole('button', { name: 'Sign out' })).toBeVisible();
  await page.getByRole('button', { name: /^back/i }).click();

  await page.locator('.sidebar-computers').getByRole('button', { name: 'Home NAS' }).click();
  const view = page.getByRole('region', { name: 'Remote computer' });
  await expect(view.getByRole('alert')).toContainText('This computer doesn’t trust Home NAS yet');
  await expect(view.getByRole('alert')).toContainText('Both computers must trust each other');
  await expect(page.locator('#toast')).toBeHidden();

  await page.getByRole('button', { name: 'Conversation view' }).click();
  await page.locator('.sidebar-computers').getByRole('button', { name: 'Studio iMac' }).click();
  await view.getByRole('button', { name: /Write release notes/ }).click();
  await expect(view.getByRole('log', { name: 'Conversation' })).toContainText('plain text');
  await expect(view.locator('.remote-msg img')).toHaveCount(0);
  await expect(view.locator('.remote-msg')).toHaveCount(1);
  expect(await page.evaluate(() => (window as any).__xss)).toBeUndefined();
});

async function signedInView(page: Page) {
  const panel = await openRemoteSettings(page);
  await panel.getByRole('button', { name: 'Sign in with GitHub' }).click();
  await expect(panel.getByRole('button', { name: 'Sign out' })).toBeVisible();
  await page.getByRole('button', { name: /^back/i }).click();
  await page.locator('.sidebar-computers').getByRole('button', { name: 'Studio iMac' }).click();
  return page.getByRole('region', { name: 'Remote computer' });
}

test('streamed text updates from the live delta without fetching history', async ({ page }) => {
  const view = await signedInView(page);
  await view.getByRole('button', { name: /Fix the flaky export test/ }).click();
  await expect(view.getByRole('log', { name: 'Conversation' })).toContainText('The test races the file watcher');
  await view.getByRole('button', { name: 'Load earlier' }).click();
  await expect(view.getByRole('log', { name: 'Conversation' })).toContainText('Earlier: the export test is flaky.');
  await expect(view.getByRole('button', { name: 'Load earlier' })).toHaveCount(0);
  await view.getByRole('textbox', { name: 'Message' }).fill('go');
  await view.getByRole('button', { name: 'Send' }).click();
  const log = view.getByRole('log', { name: 'Conversation' });
  await expect(log).toContainText('Working on it');
  const gets = await page.evaluate(() => (window as any).__remoteGets);
  await expect(log).toContainText('almost there');
  expect(await page.evaluate(() => (window as any).__remoteGets)).toBe(gets);
  await expect(log).toContainText('Done: go');
});

test('fast task switching never strands the new task on Loading', async ({ page }) => {
  const view = await signedInView(page);
  await view.getByRole('button', { name: /Write release notes/ }).click(); // answers after 300 ms
  await view.getByRole('button', { name: /Fix the flaky export test/ }).click();
  const log = view.getByRole('log', { name: 'Conversation' });
  await expect(log).toContainText('The test races the file watcher');
  await page.waitForTimeout(500);
  await expect(log).toContainText('The test races the file watcher');
  await expect(log).not.toContainText('Loading conversation');
});

test('an offline computer shows a banner and keeps the draft', async ({ page }) => {
  const view = await signedInView(page);
  await view.getByRole('button', { name: /Fix the flaky export test/ }).click();
  const box = view.getByRole('textbox', { name: 'Message' });
  await box.fill('half-written thought');
  await page.evaluate(() => (window as any).__remoteOnline('dev-studio', false));
  await expect(view.getByText(/Studio iMac is offline/)).toBeVisible();
  await expect(view.getByRole('log', { name: 'Conversation' })).toContainText('The test races the file watcher');
  await expect(box).toHaveValue('half-written thought');
  await page.evaluate(() => (window as any).__remoteOnline('dev-studio', true));
  await expect(view.getByText(/Studio iMac is offline/)).toHaveCount(0);
});

test('sign-in can be cancelled', async ({ page }) => {
  await page.addInitScript(() => { (window as any).__signInDelay = 60_000; });
  const panel = await openRemoteSettings(page);
  await panel.getByRole('button', { name: 'Sign in with GitHub' }).click();
  await expect(panel.getByLabel('Device code')).toHaveText('WDJB-MJHT');
  await expect(panel.getByText(/code expires at/)).toBeVisible();
  await panel.getByRole('button', { name: 'Cancel' }).click();
  await expect(panel.getByLabel('Device code')).toHaveCount(0);
  await expect(panel.getByRole('button', { name: 'Sign in with GitHub' })).toBeEnabled();
  await expect(panel.getByRole('alert')).toHaveCount(0);
});

test('the trust toggle shows without host mode', async ({ page }) => {
  const panel = await openRemoteSettings(page);
  await panel.getByRole('button', { name: 'Sign in with GitHub' }).click();
  await expect(panel.getByRole('checkbox', { name: 'Allow remote control of this computer' })).not.toBeChecked();
  await expect(panel.getByRole('checkbox', { name: 'Trusted: Studio iMac' })).toBeChecked();
});
