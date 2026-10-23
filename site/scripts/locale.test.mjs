import test from 'node:test';
import assert from 'node:assert/strict';
import { localeRedirect } from '../functions/_middleware.js';

test('accept-language ja redirects', () => {
  assert.deepEqual(localeRedirect('/docs/x', '', '', 'ja,en;q=0.8'), { redirect: '/ja/docs/x' });
});
test('cookie overrides accept-language', () => {
  assert.deepEqual(localeRedirect('/docs/x', '', 'a=b; NEXT_LOCALE=en', 'ja'), { redirect: '/en/docs/x' });
});
test('root goes to bare locale', () => {
  assert.deepEqual(localeRedirect('/', '', '', ''), { redirect: '/en' });
});
test('locale path with different cookie sets cookie', () => {
  assert.deepEqual(localeRedirect('/ja/docs', '', 'NEXT_LOCALE=en', ''), { setCookie: 'ja' });
});
test('matching cookie is a no-op', () => {
  assert.equal(localeRedirect('/en/docs', '', 'NEXT_LOCALE=en', ''), null);
});
test('search preserved', () => {
  assert.deepEqual(localeRedirect('/docs/x', '?a=1', '', 'ja'), { redirect: '/ja/docs/x?a=1' });
});
test('q-ordering picks higher q', () => {
  assert.deepEqual(localeRedirect('/x', '', '', 'en;q=0.5,ja;q=0.9'), { redirect: '/ja/x' });
});
