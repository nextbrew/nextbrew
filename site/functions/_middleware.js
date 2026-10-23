// Port of Nextra's locale proxy (static export can't run Next middleware).
const HAS_LOCALE = /^\/(en|ja)(\/|$)/;

function cookieLocale(cookieHeader) {
  const m = /(?:^|;\s*)NEXT_LOCALE=(en|ja)(?:;|$)/.exec(cookieHeader || '');
  return m && m[1];
}

function headerLocale(acceptLanguage) {
  const langs = (acceptLanguage || '').split(',').map((part, i) => {
    const [tag, ...params] = part.trim().split(';');
    const q = params.map((p) => /^\s*q=([\d.]+)/.exec(p)).find(Boolean);
    return { tag: tag.toLowerCase(), q: q ? parseFloat(q[1]) : 1, i };
  }).filter((l) => l.tag && l.q > 0).sort((a, b) => b.q - a.q || a.i - b.i);
  return langs.length && langs[0].tag.startsWith('ja') ? 'ja' : 'en';
}

export function localeRedirect(pathname, search, cookieHeader, acceptLanguage) {
  const cookie = cookieLocale(cookieHeader);
  if (!HAS_LOCALE.test(pathname)) {
    const locale = cookie || headerLocale(acceptLanguage);
    return { redirect: `/${locale}${pathname === '/' ? '' : pathname}${search}` };
  }
  const requested = pathname.split('/')[1];
  return requested === cookie ? null : { setCookie: requested };
}

export async function onRequest(context) {
  const { request } = context;
  const url = new URL(request.url);
  const r = localeRedirect(url.pathname, url.search, request.headers.get('Cookie'), request.headers.get('Accept-Language'));
  if (r?.redirect) return Response.redirect(new URL(r.redirect, url).toString(), 302);
  const res = await context.next();
  if (!r?.setCookie) return res;
  const out = new Response(res.body, res);
  out.headers.append('Set-Cookie', `NEXT_LOCALE=${r.setCookie}; Path=/; Max-Age=31536000; SameSite=Lax`);
  return out;
}
