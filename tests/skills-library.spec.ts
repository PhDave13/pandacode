import { expect, test, type Page } from '@playwright/test';

/* Settings → Skills & rules, mounted on its own page over the preview library
 * (src/preview-skills.ts), so it runs whether or not Settings has wired the section yet. */
async function open(page: Page): Promise<void> {
  await page.route('**/skills-library-harness.html', route => route.fulfill({
    contentType: 'text/html',
    body: `<!doctype html><html><head><link rel="stylesheet" href="/src/styles.css"></head><body><main id="host" class="settings-page-root"></main>
      <script type="module">
        import { mountSkillsLibrary } from '/src/skills-library.ts';
        import { createPreviewSkills } from '/src/preview-skills.ts';
        const skills = createPreviewSkills();
        window.__skills = skills;
        const call = async (command, args = {}) => skills.call(command, args);
        mountSkillsLibrary(document.getElementById('host'), { call, projects: () => [{ id: 'orbit', name: 'Orbit' }] });
      </script></body></html>`,
  }));
  await page.goto('/skills-library-harness.html');
  await expect(page.getByRole('list', { name: 'Library' })).toBeVisible();
}
const item = (page: Page, name: string) => page.locator('.skills-item', { hasText: name });
const status = (page: Page, row: string, column: number) => page.getByRole('table', { name: 'Skills by harness' }).getByRole('row', { name: new RegExp(row) }).locator('td').nth(column);

test('activating a skill for Claude Code and Codex shows in the matrix, and drift is reported', async ({ page }) => {
  await open(page);
  await page.getByRole('checkbox', { name: 'Release notes: Claude Code · User' }).check();
  await expect(page.getByRole('status')).toHaveText('Active for Claude Code.');
  await page.getByRole('checkbox', { name: 'Release notes: Codex · User' }).check();
  await expect(status(page, 'Release notes', 0)).toHaveText('Active');
  await expect(status(page, 'Release notes', 1)).toHaveText('Active');
  await expect(page.locator('.skills-unmanaged')).toContainText('~/.claude/skills/pdf-tools');

  await page.evaluate(() => (window as any).__skills.edit('release-notes', 'claude', 'user'));
  await page.getByRole('combobox', { name: 'Project for project scope' }).selectOption('orbit');
  await expect(status(page, 'Release notes', 0)).toHaveText('Edited on disk');
  await page.getByRole('checkbox', { name: 'Release notes: Claude Code · User' }).uncheck();
  await expect(page.getByRole('status')).toContainText('Left 1 edited file in place');
  await expect(page.getByRole('button', { name: 'Remove Release notes' })).toBeEnabled();
  await page.getByRole('button', { name: 'Remove Release notes' }).click();
  await expect(page.getByRole('alert').filter({ hasText: 'Turn Release notes off everywhere' })).toBeVisible();
});

test('project-scope rules and safety findings need acknowledgement before activation', async ({ page }) => {
  await open(page);
  await page.getByRole('combobox', { name: 'Project for project scope' }).selectOption('orbit');
  await page.getByRole('checkbox', { name: 'No force push: Codex · Project' }).check();
  await expect(page.getByRole('status')).toHaveText('Active for Codex.');

  await page.getByRole('textbox', { name: 'Folder or file path' }).fill('/Users/you/skills/setup');
  await page.getByRole('button', { name: 'Import' }).click();
  await expect(page.getByRole('status')).toHaveText('Imported setup. 1 safety finding to review.');
  await expect(page.getByRole('list', { name: 'Safety findings for setup' })).toContainText('Download piped into a shell');
  const toggle = page.getByRole('checkbox', { name: 'setup: Claude Code · User' });
  await toggle.click();
  await expect(toggle).not.toBeChecked();
  await item(page, 'setup').getByRole('button', { name: 'Activate anyway' }).click();
  await expect(page.getByRole('checkbox', { name: 'setup: Claude Code · User' })).toBeChecked();
});

test('GitHub import validates the repository and records the commit', async ({ page }) => {
  await open(page);
  await page.getByRole('combobox', { name: 'Source' }).selectOption('github');
  await page.getByRole('button', { name: 'Import' }).click();
  await expect(page.getByRole('alert').filter({ hasText: 'owner/name' })).toBeVisible();
  await page.getByRole('textbox', { name: 'Repository', exact: true }).fill('acme/skills');
  await page.getByRole('textbox', { name: 'Path in repository' }).fill('skills/pdf');
  await page.getByRole('button', { name: 'Import' }).click();
  await expect(item(page, 'pdf')).toContainText('GitHub · acme/skills/skills/pdf @ a1b2c3d');
  await expect(page.getByRole('textbox', { name: 'Repository', exact: true })).toHaveValue('');
});
