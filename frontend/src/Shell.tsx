import type { ReactNode } from 'react'
import { Link } from 'react-router-dom'

export function Logo({ className = 'h-7 w-7' }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 32 32" aria-hidden="true">
      <rect width="32" height="32" rx="8" fill="#0B0B0B" />
      <path d="M10 8h9l4 4v12H10z" fill="none" stroke="#fff" strokeWidth="1.6" strokeLinejoin="round" opacity=".85" />
      <path d="M13 15h7M13 18.5h7" stroke="#fff" strokeWidth="1.4" strokeLinecap="round" opacity=".55" />
      <circle cx="22.5" cy="22.5" r="4.2" fill="#D85A30" />
    </svg>
  )
}

// Where to chip in: a redirect to the payment page, so the page can change
// without this changing. The server's out-of-credit message names the same
// address (src/pipeline.rs), and PaperPage turns it into a link.
export const DONATE = 'https://aero-sg.us.to'

export function Shell({ children }: { children: ReactNode }) {
  return (
    <>
      <div className="grid-bg" />
      <header className="sticky top-0 z-30 border-b border-line bg-paper/85 backdrop-blur">
        <div className="relative z-10 mx-auto flex h-[60px] max-w-[1100px] items-center justify-between px-4 sm:px-6">
          <Link to="/" className="flex items-center gap-2.5 text-[15px] font-bold tracking-tight text-ink no-underline">
            <Logo />
            paper-bro
          </Link>
          <span className="eyebrow hidden sm:inline">arXiv papers → interactive courses</span>
        </div>
      </header>
      <main className="relative z-10 mx-auto max-w-[1100px] px-4 pb-20 sm:px-6">{children}</main>
      <footer className="relative z-10 pb-12 text-center text-[13px] text-fg-3">
        Courses are written by an AI model from the paper and hosted on{' '}
        <a className="text-fg-2 underline-offset-2 hover:underline" href="https://github.com/paper-bro/paper-bro.github.io">GitHub Pages</a>. Papers come from{' '}
        <a className="text-fg-2 underline-offset-2 hover:underline" href="https://arxiv.org">arXiv</a>; paper-bro isn't affiliated with it.
        Writing courses costs real money, so if paper-bro is useful to you, you can{' '}
        <a className="text-fg-2 underline-offset-2 hover:underline" href={DONATE} target="_blank" rel="noopener">chip in</a>.
      </footer>
    </>
  )
}
