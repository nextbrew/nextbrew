import test from 'node:test';
import assert from 'node:assert/strict';
import { localeRedirect } from '../functions/_middleware.js';

test('defaults to English', () => {
  assert.deepEqual(localeRedirect('/docs/x', '', ''), { redirect: '/en/docs/x' });
});
test('cookie picks the language', () => {
  assert.deepEqual(localeRedirect('/docs/x', '', 'a=b; NEXT_LOCALE=ja'), { redirect: '/ja/docs/x' });
});
test('root serves English in place', () => {
  assert.deepEqual(localeRedirect('/', '', ''), { rewrite: '/en' });
});
test('root with ja cookie redirects', () => {
  assert.deepEqual(localeRedirect('/', '', 'NEXT_LOCALE=ja'), { redirect: '/ja' });
});
test('locale path with different cookie sets cookie', () => {
  assert.deepEqual(localeRedirect('/ja/docs', '', 'NEXT_LOCALE=en'), { setCookie: 'ja' });
});
test('matching cookie is a no-op', () => {
  assert.equal(localeRedirect('/en/docs', '', 'NEXT_LOCALE=en'), null);
});
test('search preserved', () => {
  assert.deepEqual(localeRedirect('/docs/x', '?a=1', 'NEXT_LOCALE=ja'), { redirect: '/ja/docs/x?a=1' });
});
