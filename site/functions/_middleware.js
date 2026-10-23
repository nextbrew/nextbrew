// Port of Nextra's locale proxy (static export can't run Next middleware).
const HAS_LOCALE = /^\/(en|ja)(\/|$)/;

function cookieLocale(cookieHeader) {
  const m = /(?:^|;\s*)NEXT_LOCALE=(en|ja)(?:;|$)/.exec(cookieHeader || '');
  return m && m[1];
}

export function localeRedirect(pathname, search, cookieHeader) {
  const cookie = cookieLocale(cookieHeader);
  if (!HAS_LOCALE.test(pathname)) {
    // English unless the visitor picked a language with the switcher.
    const locale = cookie || 'en';
    return { redirect: `/${locale}${pathname === '/' ? '' : pathname}${search}` };
  }
  const requested = pathname.split('/')[1];
  return requested === cookie ? null : { setCookie: requested };
}

export async function onRequest(context) {
  const { request } = context;
  const url = new URL(request.url);
  const r = localeRedirect(url.pathname, url.search, request.headers.get('Cookie'));
  if (r?.redirect) return Response.redirect(new URL(r.redirect, url).toString(), 302);
  const res = await context.next();
  if (!r?.setCookie) return res;
  const out = new Response(res.body, res);
  out.headers.append('Set-Cookie', `NEXT_LOCALE=${r.setCookie}; Path=/; Max-Age=31536000; SameSite=Lax`);
  return out;
}
