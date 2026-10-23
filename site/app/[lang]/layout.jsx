import { GeistMono } from 'geist/font/mono'
import { GeistSans } from 'geist/font/sans'
import { Head } from 'nextra/components'
import { getPageMap } from 'nextra/page-map'
import { Footer, Layout, LocaleSwitch, Navbar } from 'nextra-theme-docs'
import 'nextra-theme-docs/style.css'
import { Logo } from '../../components/brand'
import { text } from '../../components/text'
import './styles.css'

export const metadata = {
  metadataBase: new URL('https://nextbrew.dev'),
  title: { template: '%s – nextbrew', default: 'nextbrew' },
  description: 'A fast, Homebrew-compatible package manager for macOS and Linux.',
  icons: { icon: '/favicon.svg', apple: '/apple-touch-icon.png' },
}

const REPO = 'https://github.com/nextbrew/nextbrew'

export default async function RootLayout({ children, params }) {
  const { lang } = await params
  const t = text[lang].footer
  const footer = (
    <Footer>
      <div className="nb-footer">
        <div><Logo /><p>{t.credits}</p></div>
        <div><h5>{t.docs}</h5>
          <a href={`/${lang}/docs/installation`}>{t.installation}</a>
          <a href={`/${lang}/docs/quick-start`}>{t.quickStart}</a>
          <a href={`/${lang}/docs/migrating-from-homebrew`}>{t.migrating}</a></div>
        <div><h5>{t.project}</h5>
          <a href={REPO}>GitHub</a>
          <a href={`${REPO}/releases`}>{t.releases}</a>
          <a href={`/${lang}/benchmarks`}>{t.benchmarks}</a></div>
        <div><h5>{t.language}</h5>
          <a href="/en">English</a>
          <a href="/ja">日本語</a></div>
      </div>
    </Footer>
  )
  return (
    <html lang={lang} dir="ltr" suppressHydrationWarning className={`${GeistSans.variable} ${GeistMono.variable}`}>
      <Head />
      <body>
        <Layout
          navbar={<Navbar logo={<Logo />} projectLink={REPO}><LocaleSwitch /></Navbar>}
          footer={footer}
          pageMap={await getPageMap(`/${lang}`)}
          docsRepositoryBase={`${REPO}/tree/main/site`}
          i18n={[{ locale: 'en', name: 'English' }, { locale: 'ja', name: '日本語' }]}
        >
          {children}
        </Layout>
      </body>
    </html>
  )
}
