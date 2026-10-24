import { expect, test } from '@playwright/test';

// Issue #123: the picker lists only installed harnesses. The preview marks
// Antigravity and Oh My Pi as not installed.
test('the harness picker hides harnesses that are not installed', async ({ page }) => {
  await page.goto('/');
  await page.locator('.new-task-button').click();
  await page.getByRole('button', { name: 'Choose harness and model', exact: true }).click();
  const picker = page.getByRole('dialog', { name: 'Harness and model', exact: true });
  await expect(picker.getByRole('region', { name: 'Grok Build' })).toBeVisible();
  await expect(picker.locator('[data-provider="antigravity"], [data-provider="omp"]')).toHaveCount(0);
  await expect(picker).not.toContainText('Antigravity');
  await expect(picker).not.toContainText('Oh My Pi');
});
